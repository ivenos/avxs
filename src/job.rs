use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::Semaphore;

use crate::audio;
use crate::config::{Config, TargetQualityConfig, VideoMode};
use crate::encode::{self, EncodeOptions};
use crate::ffms2::{self, Crop};
use crate::resume::{CrfCache, DoneFile, SceneEntry, TempDir};
use crate::scanner::Job;
use crate::scene;
use crate::target_quality;
use crate::workers;

pub struct JobContext {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
}

/// Per-job context shared by every chunk worker, cloned as one Arc per task
/// instead of threading a dozen values through each spawn.
struct WorkerCtx {
    source: PathBuf,
    index: PathBuf,
    temp_dir: PathBuf,
    config: Arc<Config>,
    opts: Arc<EncodeOptions>,
    tq: Option<TargetQualityConfig>,
    tq_display_model: Option<String>,
    tq_gpu_id: Option<u32>,
    source_width: u32,
    source_height: u32,
    crf_cache: Option<Arc<CrfCache>>,
    threads_per_worker: usize,
    stem: String,
    total_chunks: usize,
    total_frames: u64,
    /// Cumulative source byte sizes by frame for the size cap; empty when unused.
    source_byte_index: Arc<Vec<u64>>,
}

pub async fn run(job: &Job, ctx: &JobContext) -> Result<()> {
    let config = Arc::new(Config::from_file(&job.encode_toml)?);

    let stem = job.stem();

    wait_for_stable(&job.source_file, stem).await?;

    let temp = TempDir::for_video(&ctx.output_dir, stem);
    temp.create_dirs()?;

    if config.avxs.video == VideoMode::Copy {
        return run_copy(job, ctx, &config, stem, &temp).await;
    }

    if !temp.index_path.exists() {
        tracing::info!("[{stem}] indexing");
        ffms2::run_ffmsindex(&job.source_file, &temp.index_path).await?;
        tracing::info!("[{stem}] indexing done");
    } else {
        tracing::info!("[{stem}] reusing existing index");
    }

    let source_path = job.source_file.clone();
    let index_path  = temp.index_path.clone();
    let video_info  = tokio::task::spawn_blocking(move || {
        ffms2::VideoSource::open(&source_path, &index_path, ffms2::OpenOpts::default())
            .map(|vs| vs.info.clone())
    })
    .await
    .context("spawn_blocking VideoSource")??;

    let threads_per_worker = config.encoder_params
        .get("lp")
        .and_then(|v| match v {
            toml::Value::Integer(i) => usize::try_from(*i).ok(),
            toml::Value::String(s)  => s.parse().ok(),
            _ => None,
        })
        .filter(|&n| n > 0)
        .unwrap_or(6);
    let num_workers = workers::calculate(&video_info, stem, threads_per_worker);

    // FPS resolved before crop/keyint so duration_secs is valid
    let (fps_num, fps_den) = probe_fps(&job.source_file).await?;
    let fps = fps_num as f64 / fps_den as f64;

    let hdr_args: Vec<String> = if config.avxs.hdr {
        let hdr = crate::hdr::detect(&job.source_file).await?;
        if hdr.hdr_type == "Dolby Vision" || hdr.hdr_type == "HDR10+" {
            tracing::info!(
                "[{stem}] HDR: {} (not supported, passing HDR10 fallback metadata)",
                hdr.hdr_type
            );
        } else {
            tracing::info!("[{stem}] HDR: {}", hdr.hdr_type);
        }
        hdr.encoder_args()
    } else {
        Vec::new()
    };

    let crop_str: Option<String> = if config.avxs.crop {
        let duration_secs = video_info.num_frames as f64 / fps;
        crate::crop::detect(&job.source_file, duration_secs, &temp.crop_cache, stem).await?
    } else {
        None
    };

    let (scale_target, crop, scene_vf) = compute_output_params(
        video_info.width,
        video_info.height,
        crop_str.as_deref(),
        config.avxs.scale,
        stem,
    );

    let auto_keyint: Option<u32> = if config.avxs.keyint {
        let ki = (fps * 5.0).round().max(1.0) as u32;
        tracing::info!("[{stem}] auto-keyint: {ki} ({fps:.3} fps, keyframe every ~5s)");
        Some(ki)
    } else {
        None
    };

    let encode_opts = Arc::new(EncodeOptions {
        hdr_args,
        keyint: auto_keyint,
        scale: scale_target,
        crop,
        fps_num,
        fps_den,
        target_bit_depth: config.avxs.bit_depth,
    });

    // Effective encoder args, shared by the summary log and the cache fingerprint.
    let merged_args = encode::merged_encoder_args(&config, &encode_opts);

    // Discard cached scenes/chunks if the encode profile changed since the last run,
    // so a resumed encode never mixes chunks from different settings.
    let fingerprint = profile_fingerprint(&merged_args, &encode_opts, &config.scene_detection, config.target_quality.as_ref());
    invalidate_stale_cache(&temp, &fingerprint, stem)?;

    let scenes: Vec<SceneEntry> = if temp.scenes_path.exists() {
        tracing::info!("[{stem}] reusing scenes.json");
        crate::resume::read_scenes(&temp.scenes_path)?
    } else {
        tracing::info!("[{stem}] scene detection");
        let scenes = scene::detect(
            &job.source_file,
            &config.scene_detection,
            scene_vf.as_deref(),
            fps,
        )
        .await?;
        crate::resume::write_scenes(&temp.scenes_path, &scenes)?;
        tracing::info!("[{stem}] {} chunks", scenes.len());
        scenes
    };

    // clamp to FFMS2 frame count - scene detector may overcount on broken remuxes
    let ffms2_frames = video_info.num_frames as u64;
    let scenes: Vec<SceneEntry> = scenes
        .into_iter()
        .filter_map(|mut s| {
            if s.start_frame >= ffms2_frames {
                tracing::warn!(
                    "[{stem}] dropping scene {} (start {} >= FFMS2 frame count {})",
                    s.index, s.start_frame, ffms2_frames
                );
                return None;
            }
            if s.end_frame >= ffms2_frames {
                tracing::warn!(
                    "[{stem}] clamping scene {} end_frame {} to {}",
                    s.index, s.end_frame, ffms2_frames - 1
                );
                s.end_frame = ffms2_frames - 1;
            }
            Some(s)
        })
        .collect();

    let total_chunks = scenes.len();
    let total_frames: u64 = scenes.iter().map(|s| s.frame_count()).sum();

    let summary: Vec<String> = merged_args
        .chunks(2)
        .filter_map(|pair| match pair {
            [k, v] => Some(format!("{}={}", k.trim_start_matches('-'), v)),
            _      => None,
        })
        .collect();
    tracing::info!("[{stem}] encoder args: {}", summary.join(" "));

    let audio_plan = audio::plan(&job.source_file, &config.audio).await?;
    for line in audio_plan.summary_lines() {
        tracing::info!("[{stem}] audio {line}");
    }

    // Target quality: probe the metric tool once and pick the CVVDP display model.
    // FFVship compares at the source resolution (a scaled-down encode is resized up to
    // it), so the display model is keyed off the source/crop height, not the scaled output.
    let reference_height = encode_opts.crop.map(|c| c.h).unwrap_or(video_info.height);
    let (tq_display_model, tq_gpu_id, crf_cache): (Option<String>, Option<u32>, Option<Arc<CrfCache>>) =
        if let Some(tq) = &config.target_quality {
            let gpu = target_quality::ensure_available().await?;
            let hlg = target_quality::hdr_args_are_hlg(&encode_opts.hdr_args);
            let display_model = target_quality::display_model_for(reference_height, config.avxs.hdr, hlg);
            tracing::info!(
                "[{stem}] target quality: JOD {} floor (display {display_model}, {}, crf {}-{}, {}-{} probes, probe preset {}, max {}% size)",
                tq.jod, gpu.describe(), tq.min_crf, tq.max_crf, tq.min_probes, tq.max_probes, tq.probe_preset, tq.max_encoded_percent
            );
            if config.encoder_params.contains_key("crf") {
                tracing::info!("[{stem}] target quality: crf in encoder_params used only as a probe seed");
            }
            (Some(display_model.to_string()), Some(gpu.id), Some(Arc::new(CrfCache::load_or_create(&temp.tq_path)?)))
        } else {
            (None, None, None)
        };

    tracing::info!("[{stem}] encoding: {total_chunks} chunks, {num_workers} worker(s)");

    let done               = Arc::new(DoneFile::load_or_create(&temp.done_path)?);
    let semaphore          = Arc::new(Semaphore::new(num_workers));
    let completed_chunks   = Arc::new(AtomicUsize::new(0));
    let completed_frames   = Arc::new(AtomicU64::new(0));
    let mut set = tokio::task::JoinSet::new();

    // Actual source bytes per frame for the size cap (target quality only).
    let source_byte_index = if config.target_quality.is_some() {
        Arc::new(probe_source_byte_index(&job.source_file).await)
    } else {
        Arc::new(Vec::new())
    };

    let wctx = Arc::new(WorkerCtx {
        source: job.source_file.clone(),
        index: temp.index_path.clone(),
        temp_dir: temp.path.clone(),
        config: Arc::clone(&config),
        opts: Arc::clone(&encode_opts),
        tq: config.target_quality.clone(),
        tq_display_model,
        tq_gpu_id,
        source_width: video_info.width,
        source_height: video_info.height,
        crf_cache,
        threads_per_worker,
        stem: stem.to_owned(),
        total_chunks,
        total_frames,
        source_byte_index,
    });

    for scene in &scenes {
        let chunk_key  = scene.padded_index();
        let chunk_path = temp.chunk_path(&chunk_key);
        let scene      = scene.clone();

        if done.is_done(&chunk_key, &chunk_path).await {
            completed_chunks.fetch_add(1, Ordering::Relaxed);
            completed_frames.fetch_add(scene.frame_count(), Ordering::Relaxed);
            tracing::debug!("[{stem}] chunk {chunk_key} already done");
            continue;
        }

        let w                = Arc::clone(&wctx);
        let sem              = semaphore.clone();
        let done             = done.clone();
        let completed_chunks = completed_chunks.clone();
        let completed_frames = completed_frames.clone();

        set.spawn(async move {
            let _permit = sem.acquire().await.context("acquire semaphore")?;

            let scene_frames = scene.frame_count();
            let crf_override = resolve_crf(&w, &chunk_key, &scene).await?;

            let overrides  = encode::EncodeOverrides { crf: crf_override, preset: None };
            let t0         = std::time::Instant::now();
            let source     = w.source.clone();
            let index      = w.index.clone();
            let config     = Arc::clone(&w.config);
            let opts       = Arc::clone(&w.opts);
            let size_bytes = tokio::task::spawn_blocking(move || {
                encode::encode_chunk(source, index, scene, chunk_path, &config, &opts, overrides)
            })
            .await
            .context("spawn_blocking encode_chunk")??;

            let enc_fps = scene_frames as f64 / t0.elapsed().as_secs_f64();
            done.mark_done(&chunk_key, scene_frames, size_bytes).await?;

            let n_chunks = completed_chunks.fetch_add(1, Ordering::Relaxed) + 1;
            let n_frames = completed_frames.fetch_add(scene_frames, Ordering::Relaxed) + scene_frames;
            let pct      = n_frames * 100 / w.total_frames;
            tracing::info!(
                "[{}] chunk {n_chunks}/{} - {pct}% - {enc_fps:.1} fps - {:.1} MB",
                w.stem, w.total_chunks, size_bytes as f64 / 1_048_576.0
            );

            anyhow::Ok(())
        });
    }

    while let Some(res) = set.join_next().await {
        res.context("chunk task join")??;
    }

    tracing::info!("[{stem}] merging chunks");
    let video_only  = ctx.output_dir.join(format!(".avxs_{stem}_video.mkv"));
    let chunk_paths: Vec<PathBuf> =
        scenes.iter().map(|s| temp.chunk_path(&s.padded_index())).collect();
    encode::concat_chunks(&chunk_paths, &video_only, &temp.path).await?;

    tracing::info!("[{stem}] processing audio");
    finalize(job, ctx, &config, &temp, &audio_plan, &video_only, true).await
}

/// Shared tail of run/run_copy: process audio, mux, validate, archive source, clean up.
/// `mux_video` is the encoded video (run) or the untouched source (run_copy);
/// `remove_mux_video` deletes it afterwards when it is a temp file.
async fn finalize(
    job: &Job,
    ctx: &JobContext,
    config: &Config,
    temp: &TempDir,
    audio_plan: &audio::AudioPlan,
    mux_video: &Path,
    remove_mux_video: bool,
) -> Result<()> {
    let stem = job.stem();
    let audio_path = audio::process_plan(&job.source_file, &temp.path, audio_plan).await?;

    let subtitle_sel = crate::subtitle::select_tracks(&job.source_file, &config.subtitles).await?;

    let final_output = ctx.output_dir.join(format!("{stem}.mkv"));
    tracing::info!("[{stem}] muxing to {}", final_output.display());
    audio::mux_final(mux_video, &audio_path, &job.source_file, &final_output, &subtitle_sel).await?;

    if remove_mux_video {
        let _ = std::fs::remove_file(mux_video);
    }

    tracing::info!("[{stem}] validating output");
    encode::validate_output(&final_output).await?;

    let processed_dir = crate::scanner::ensure_processed_dir(&ctx.input_dir)?;
    let dest = processed_dir.join(job.source_file.file_name().unwrap());
    std::fs::rename(&job.source_file, &dest)
        .with_context(|| format!("move source: {} to {}", job.source_file.display(), dest.display()))?;

    if !config.avxs.keep_temp {
        std::fs::remove_dir_all(&temp.path)
            .with_context(|| format!("remove temp dir: {}", temp.path.display()))?;
    }

    tracing::info!("[{stem}] done");
    Ok(())
}

/// Target-quality CRF for a chunk: cached value, else probe-and-solve (then cache it).
/// Returns None when target quality is not active.
async fn resolve_crf(w: &WorkerCtx, chunk_key: &str, scene: &SceneEntry) -> Result<Option<f64>> {
    let (Some(tq), Some(display_model), Some(gpu_id), Some(cache)) =
        (&w.tq, &w.tq_display_model, &w.tq_gpu_id, &w.crf_cache) else {
        return Ok(None);
    };
    if let Some(c) = cache.get(chunk_key).await {
        tracing::info!("[{}] chunk {chunk_key} using cached target crf {c}", w.stem);
        return Ok(Some(c));
    }

    let source       = w.source.clone();
    let index        = w.index.clone();
    let temp_dir     = w.temp_dir.clone();
    let config       = Arc::clone(&w.config);
    let opts         = Arc::clone(&w.opts);
    let tq            = tq.clone();
    let display_model = display_model.clone();
    let gpu_id        = *gpu_id;
    let source_width  = w.source_width;
    let source_height = w.source_height;
    let scene         = scene.clone();
    let stem          = w.stem.clone();
    let n_threads     = w.threads_per_worker;
    let byte_index    = Arc::clone(&w.source_byte_index);
    let res = tokio::task::spawn_blocking(move || {
        let ctx = target_quality::ProbeContext {
            source: &source, index: &index, temp_dir: &temp_dir,
            config: &config, opts: &opts, tq: &tq,
            display_model: &display_model, gpu_id, source_width, source_height,
            n_threads, stem: &stem, source_byte_index: &byte_index,
        };
        target_quality::solve_chunk_crf(&ctx, &scene)
    })
    .await
    .context("spawn_blocking solve_chunk_crf")??;

    cache.insert(chunk_key, res.crf).await?;
    match res.outcome {
        target_quality::SolveOutcome::Met => tracing::info!(
            "[{}] chunk {chunk_key} target crf {} (JOD {:.3}, {:.0}% size)",
            w.stem, res.crf, res.jod, res.size_pct
        ),
        target_quality::SolveOutcome::CapBinding => tracing::warn!(
            "[{}] chunk {chunk_key} crf {} capped by max_encoded_percent (JOD {:.3} below floor, {:.0}% size)",
            w.stem, res.crf, res.jod, res.size_pct
        ),
        target_quality::SolveOutcome::FloorUnreachable => tracing::warn!(
            "[{}] chunk {chunk_key} JOD floor unreachable, using crf {} (JOD {:.3})",
            w.stem, res.crf, res.jod
        ),
    }
    Ok(Some(res.crf))
}

pub fn handle_failure(_job: &Job, ctx: &JobContext, stem: &str, err: &anyhow::Error) {
    tracing::error!("[{stem}] job failed - source kept, temp dir preserved\n{err:#}");

    let temp = TempDir::for_video(&ctx.output_dir, stem);
    if let Err(e) = temp.create_dirs() {
        tracing::warn!("[{stem}] could not create temp dir for failure marker: {e:#}");
    }
    if let Err(e) = std::fs::write(&temp.failed_path, format!("{err:#}")) {
        tracing::warn!("[{stem}] could not write failure marker: {e:#}");
    }
}

async fn run_copy(job: &Job, ctx: &JobContext, config: &Config, stem: &str, temp: &TempDir) -> Result<()> {
    let ignored = ignored_video_opts(&config.avxs);
    if !ignored.is_empty() {
        tracing::warn!("[{stem}] video = copy: ignoring {}", ignored.join(", "));
    }

    let audio_plan = audio::plan(&job.source_file, &config.audio).await?;
    for line in audio_plan.summary_lines() {
        tracing::info!("[{stem}] audio {line}");
    }

    tracing::info!("[{stem}] copy video, processing audio");
    finalize(job, ctx, config, temp, &audio_plan, &job.source_file, false).await
}

fn ignored_video_opts(a: &crate::config::AvxsConfig) -> Vec<&'static str> {
    let mut v = Vec::new();
    if a.hdr { v.push("hdr"); }
    if a.crop { v.push("crop"); }
    if a.keyint { v.push("keyint"); }
    if a.scale.is_some() { v.push("scale"); }
    if a.bit_depth.is_some() { v.push("bit_depth"); }
    v
}

/// Returns `(scale_target, crop, scene_vf_filter)`.
/// Crop is in source space and applied before scaling; `scale_target` is the ffmpeg
/// scale size for the cropped content. `scene_vf` mirrors crop+scale for detection.
fn compute_output_params(
    src_w: u32,
    src_h: u32,
    crop_str: Option<&str>,
    target_height: Option<u32>,
    stem: &str,
) -> (Option<(u32, u32)>, Option<Crop>, Option<String>) {
    let src_crop = crop_str.and_then(Crop::from_str);

    let (eff_w, eff_h) = match src_crop {
        Some(c) => (c.w, c.h),
        None    => (src_w, src_h),
    };

    let scale_factor: f64 = match target_height {
        Some(th) if eff_h > th => th as f64 / eff_h as f64,
        _ => 1.0,
    };

    let scale_target: Option<(u32, u32)> = (scale_factor < 1.0).then(|| {
        let tw = round_down_even((eff_w as f64 * scale_factor) as u32);
        let th = round_down_even((eff_h as f64 * scale_factor) as u32);
        tracing::info!("[{stem}] auto-scale: {eff_w}x{eff_h} to {tw}x{th} (factor {scale_factor:.4})");
        (tw, th)
    });

    let scene_vf = build_scene_vf(crop_str, scale_target);

    (scale_target, src_crop, scene_vf)
}

/// ffmpeg -vf filter for scene detection (source-space crop + optional scale).
fn build_scene_vf(crop_str: Option<&str>, scale_target: Option<(u32, u32)>) -> Option<String> {
    match (crop_str, scale_target) {
        (None,    None)         => None,
        (Some(c), None)         => Some(c.to_owned()),
        (None,    Some((w, h))) => Some(format!("scale={w}:{h}")),
        (Some(c), Some((w, h))) => Some(format!("{c},scale={w}:{h}")),
    }
}

fn round_down_even(v: u32) -> u32 {
    v & !1
}

/// Stable hash of everything that affects chunk output and scene boundaries.
/// Used to spot a changed encode profile between resumes.
fn profile_fingerprint(
    merged_args: &[String],
    opts: &EncodeOptions,
    scene_cfg: &crate::config::SceneDetectionConfig,
    tq: Option<&TargetQualityConfig>,
) -> String {
    use std::hash::{Hash, Hasher};
    let parts = [
        merged_args.join(" "),
        format!("{:?}", opts.scale),
        format!("{:?}", opts.crop),
        format!("{:?}", opts.target_bit_depth),
        format!("{scene_cfg:?}"),
        format!("{tq:?}"),
    ];
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.join("|").hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Wipes cached scenes/chunks when the profile fingerprint changed, then records the
/// new one. The frame index and crop cache survive (they depend only on the source).
fn invalidate_stale_cache(temp: &TempDir, fingerprint: &str, stem: &str) -> Result<()> {
    let prev = std::fs::read_to_string(&temp.fingerprint_path).ok();
    if prev.as_deref() == Some(fingerprint) {
        return Ok(());
    }
    if prev.is_some() {
        tracing::warn!("[{stem}] encode profile changed, discarding cached scenes and chunks");
        let _ = std::fs::remove_file(&temp.scenes_path);
        let _ = std::fs::remove_file(&temp.done_path);
        let _ = std::fs::remove_file(&temp.tq_path);
        let _ = std::fs::remove_dir_all(&temp.chunks_dir);
        temp.create_dirs()?;
    }
    std::fs::write(&temp.fingerprint_path, fingerprint)
        .with_context(|| format!("write {}", temp.fingerprint_path.display()))
}

/// Waits until file size is stable across two checks. 200ms fast-path, then 2s polling, 300s timeout.
async fn wait_for_stable(path: &Path, stem: &str) -> Result<()> {
    const TIMEOUT_SECS: u64 = 300;

    let s0 = file_size(path);
    if s0 == 0 {
        bail!("file is empty or missing: {}", path.display());
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    if file_size(path) == s0 {
        return Ok(());
    }

    tracing::info!("[{stem}] file is still being written - waiting...");
    let deadline = tokio::time::Instant::now()
        + tokio::time::Duration::from_secs(TIMEOUT_SECS);

    loop {
        let s1 = file_size(path);
        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        if file_size(path) == s1 {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "timed out after {TIMEOUT_SECS}s waiting for file to stabilize: {}",
                path.display()
            );
        }
    }
}

fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Cumulative source video byte sizes by frame, for the target-quality size cap.
/// One ffprobe pass over the packets (no decoding); empty on failure (cap disabled).
async fn probe_source_byte_index(source: &Path) -> Vec<u64> {
    #[derive(serde::Deserialize)]
    struct Packets { #[serde(default)] packets: Vec<Pkt> }
    #[derive(serde::Deserialize)]
    struct Pkt { #[serde(default)] size: Option<String> }

    let parsed: Packets = match crate::ext::ffprobe_json(
        &["-v", "error", "-select_streams", "v:0",
          "-show_entries", "packet=size", "-of", "json"],
        source,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("source packet-size probe failed: {e:#} - size cap disabled");
            return Vec::new();
        }
    };

    let mut cum = Vec::with_capacity(parsed.packets.len() + 1);
    let mut acc = 0u64;
    cum.push(0);
    for pk in &parsed.packets {
        acc += pk.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
        cum.push(acc);
    }
    cum
}

async fn probe_fps(source: &Path) -> Result<(u32, u32)> {
    #[derive(serde::Deserialize)]
    struct Probe { streams: Vec<Stream> }
    #[derive(serde::Deserialize)]
    struct Stream { avg_frame_rate: String }

    let p: Probe = crate::ext::ffprobe_json(
        &["-v", "error", "-select_streams", "v:0",
          "-show_entries", "stream=avg_frame_rate", "-of", "json"],
        source,
    )
    .await?;
    let rate = p.streams.into_iter().next()
        .map(|s| s.avg_frame_rate)
        .context("ffprobe found no video stream")?;

    if let Some((n, d)) = rate.split_once('/') {
        let n: u32 = n.trim().parse().context("parse fps numerator")?;
        let d: u32 = d.trim().parse().context("parse fps denominator")?;
        if d > 0 && n > 0 { Ok((n, d)) } else { bail!("invalid fps: {n}/{d}") }
    } else {
        let n: u32 = rate.trim().parse().context("parse fps")?;
        if n > 0 { Ok((n, 1)) } else { bail!("invalid fps: {n}") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SceneDetectionConfig;

    fn opts() -> EncodeOptions {
        EncodeOptions { fps_num: 24, fps_den: 1, ..Default::default() }
    }

    #[test]
    fn fingerprint_changes_with_profile() {
        let args = vec!["--crf".to_string(), "28".to_string()];
        let sc = SceneDetectionConfig::default();
        let base = profile_fingerprint(&args, &opts(), &sc, None);

        // identical inputs -> identical fingerprint
        assert_eq!(base, profile_fingerprint(&args, &opts(), &sc, None));

        // encoder-arg change -> different
        let args2 = vec!["--crf".to_string(), "30".to_string()];
        assert_ne!(base, profile_fingerprint(&args2, &opts(), &sc, None));

        // scale change -> different
        let mut o = opts();
        o.scale = Some((1920, 1080));
        assert_ne!(base, profile_fingerprint(&args, &o, &sc, None));

        // target_quality change -> different
        let tq = crate::config::TargetQualityConfig { jod: 9.6, ..Default::default() };
        assert_ne!(base, profile_fingerprint(&args, &opts(), &sc, Some(&tq)));
    }
}

