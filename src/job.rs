use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::Semaphore;

use crate::audio;
use crate::config::{Config, VideoMode};
use crate::encode::{self, EncodeOptions};
use crate::ffms2::{self, Crop};
use crate::paths::external_bin;
use crate::resume::{CrfCache, DoneFile, SceneEntry, TempDir};
use crate::scanner::Job;
use crate::scene;
use crate::target_quality;
use crate::vmaf;
use crate::workers;

pub struct JobContext {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
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

    // Auto-HDR
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

    // Auto-Crop
    let crop_str: Option<String> = if config.avxs.crop {
        let duration_secs = video_info.num_frames as f64 / fps;
        crate::crop::detect(&job.source_file, duration_secs, &temp.crop_cache, stem).await?
    } else {
        None
    };

    // Auto-Scale: source-space crop plus an ffmpeg scale target (crop before scale)
    let (scale_target, crop, scene_vf) = compute_output_params(
        video_info.width,
        video_info.height,
        crop_str.as_deref(),
        config.avxs.scale,
        stem,
    );

    // Auto-Keyint: ~5s keyframe distance from source FPS
    let auto_keyint: Option<u32> = if config.avxs.keyint {
        let ki = (fps * 5.0).round().max(1.0) as u32;
        tracing::info!("[{stem}] auto-keyint: {ki} ({fps:.3} fps → keyframe every ~5s)");
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

    // Scene detection
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
                    "[{stem}] clamping scene {} end_frame {} → {}",
                    s.index, s.end_frame, ffms2_frames - 1
                );
                s.end_frame = ffms2_frames - 1;
            }
            Some(s)
        })
        .collect();

    let total_chunks = scenes.len();
    let total_frames: u64 = scenes.iter().map(|s| s.frame_count()).sum();

    // Compact "key=value" summary of effective encoder args
    let summary: Vec<String> = encode::merged_encoder_args(&config, &encode_opts)
        .chunks(2)
        .filter_map(|pair| match pair {
            [k, v] => Some(format!("{}={}", k.trim_start_matches('-'), v)),
            _      => None,
        })
        .collect();
    tracing::info!("[{stem}] encoder args: {}", summary.join(" "));

    // Audio plan (logged before the encode, next to the video summary)
    let audio_plan = audio::plan(&job.source_file, &config.audio).await?;
    for line in audio_plan.summary_lines() {
        tracing::info!("[{stem}] audio {line}");
    }

    // Target quality: pick the VMAF model once (fails early if v1 is missing)
    let output_height = encode_opts.scale.map(|(_, h)| h)
        .or(encode_opts.crop.map(|c| c.h))
        .unwrap_or(video_info.height);
    let (tq_model, crf_cache): (Option<String>, Option<Arc<CrfCache>>) =
        if let Some(tq) = &config.target_quality {
            vmaf::ensure_available().await?;
            let model = vmaf::model_for_height(output_height).to_string();
            tracing::info!(
                "[{stem}] target quality: VMAF {} (model {model}, crf {}-{}, {} probes, probe preset {})",
                tq.vmaf, tq.min_crf, tq.max_crf, tq.probes, tq.probe_preset
            );
            if config.encoder_params.contains_key("crf") {
                tracing::info!("[{stem}] target quality: crf in encoder_params used only as a probe seed");
            }
            (Some(model), Some(Arc::new(CrfCache::load_or_create(&temp.tq_path)?)))
        } else {
            (None, None)
        };

    tracing::info!("[{stem}] encoding: {total_chunks} chunks, {num_workers} worker(s)");

    // Parallel chunk encoding
    let done               = Arc::new(DoneFile::load_or_create(&temp.done_path)?);
    let semaphore          = Arc::new(Semaphore::new(num_workers));
    let completed_chunks   = Arc::new(AtomicUsize::new(0));
    let completed_frames   = Arc::new(AtomicU64::new(0));
    let mut set            = tokio::task::JoinSet::new();

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

        let sem              = semaphore.clone();
        let done             = done.clone();
        let completed_chunks = completed_chunks.clone();
        let completed_frames = completed_frames.clone();
        let source           = job.source_file.clone();
        let index            = temp.index_path.clone();
        let cfg              = Arc::clone(&config);
        let opts             = Arc::clone(&encode_opts);
        let stem_owned       = stem.to_owned();
        let total_c          = total_chunks;
        let total_f          = total_frames;
        let tq               = config.target_quality.clone();
        let tq_model         = tq_model.clone();
        let crf_cache        = crf_cache.clone();
        let temp_dir         = temp.path.clone();
        let tpw              = threads_per_worker;

        set.spawn(async move {
            let _permit = sem.acquire().await.context("acquire semaphore")?;

            let scene_frames = scene.frame_count();

            let crf_override = match (&tq, &tq_model, &crf_cache) {
                (Some(tq), Some(model), Some(cache)) => {
                    if let Some(c) = cache.get(&chunk_key).await {
                        Some(c)
                    } else {
                        let s2 = source.clone();
                        let i2 = index.clone();
                        let td = temp_dir.clone();
                        let c2 = Arc::clone(&cfg);
                        let o2 = Arc::clone(&opts);
                        let tq2 = tq.clone();
                        let m2 = model.clone();
                        let scene2 = scene.clone();
                        let solved = tokio::task::spawn_blocking(move || {
                            let ctx = target_quality::ProbeContext {
                                source: &s2, index: &i2, temp_dir: &td,
                                config: &c2, opts: &o2, tq: &tq2, model: &m2, n_threads: tpw,
                            };
                            target_quality::solve_chunk_crf(&ctx, &scene2)
                        })
                        .await
                        .context("spawn_blocking solve_chunk_crf")??;
                        cache.insert(&chunk_key, solved).await?;
                        tracing::info!("[{stem_owned}] chunk {chunk_key} target crf {solved}");
                        Some(solved)
                    }
                }
                _ => None,
            };

            let overrides    = encode::EncodeOverrides { crf: crf_override, preset: None };
            let t0           = std::time::Instant::now();
            let size_bytes   = tokio::task::spawn_blocking(move || {
                encode::encode_chunk(source, index, scene, chunk_path, &cfg, &opts, overrides)
            })
            .await
            .context("spawn_blocking encode_chunk")??;

            let enc_fps = scene_frames as f64 / t0.elapsed().as_secs_f64();

            done.mark_done(&chunk_key, scene_frames, size_bytes).await?;

            let n_chunks = completed_chunks.fetch_add(1, Ordering::Relaxed) + 1;
            let n_frames = completed_frames.fetch_add(scene_frames, Ordering::Relaxed) + scene_frames;
            let pct      = n_frames * 100 / total_f;
            tracing::info!(
                "[{stem_owned}] chunk {n_chunks}/{total_c} - {pct}% - {enc_fps:.1} fps - {:.1} MB",
                size_bytes as f64 / 1_048_576.0
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
    let audio_path = audio::process_plan(&job.source_file, &temp.path, &audio_plan).await?;

    let subtitle_sel = crate::subtitle::select_tracks(&job.source_file, &config.subtitles).await?;

    let final_output = ctx.output_dir.join(format!("{stem}.mkv"));
    tracing::info!("[{stem}] muxing → {}", final_output.display());
    audio::mux_final(&video_only, &audio_path, &job.source_file, &final_output, &subtitle_sel).await?;

    let _ = std::fs::remove_file(&video_only);

    tracing::info!("[{stem}] validating output");
    encode::validate_output(&final_output).await?;

    let processed_dir = crate::scanner::ensure_processed_dir(&ctx.input_dir)?;
    let dest = processed_dir.join(job.source_file.file_name().unwrap());
    std::fs::rename(&job.source_file, &dest)
        .with_context(|| format!("move source: {} → {}", job.source_file.display(), dest.display()))?;

    if !config.avxs.keep_temp {
        std::fs::remove_dir_all(&temp.path)
            .with_context(|| format!("remove temp dir: {}", temp.path.display()))?;
    }

    tracing::info!("[{stem}] done");

    Ok(())
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
    let audio_path = audio::process_plan(&job.source_file, &temp.path, &audio_plan).await?;

    let subtitle_sel = crate::subtitle::select_tracks(&job.source_file, &config.subtitles).await?;

    let final_output = ctx.output_dir.join(format!("{stem}.mkv"));
    tracing::info!("[{stem}] muxing → {}", final_output.display());
    audio::mux_final(&job.source_file, &audio_path, &job.source_file, &final_output, &subtitle_sel).await?;

    tracing::info!("[{stem}] validating output");
    encode::validate_output(&final_output).await?;

    let processed_dir = crate::scanner::ensure_processed_dir(&ctx.input_dir)?;
    let dest = processed_dir.join(job.source_file.file_name().unwrap());
    std::fs::rename(&job.source_file, &dest)
        .with_context(|| format!("move source: {} → {}", job.source_file.display(), dest.display()))?;

    if !config.avxs.keep_temp {
        std::fs::remove_dir_all(&temp.path)
            .with_context(|| format!("remove temp dir: {}", temp.path.display()))?;
    }

    tracing::info!("[{stem}] done");
    Ok(())
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

    let scene_vf = build_scene_vf(crop_str, scale_factor, eff_w, eff_h);

    let scale_target = if scale_factor < 1.0 {
        let tw = round_down_even((eff_w as f64 * scale_factor) as u32);
        let th = round_down_even((eff_h as f64 * scale_factor) as u32);
        tracing::info!("[{stem}] auto-scale: {eff_w}×{eff_h} → {tw}×{th} (factor {scale_factor:.4})");
        Some((tw, th))
    } else {
        None
    };

    (scale_target, src_crop, scene_vf)
}

/// ffmpeg -vf filter for scene detection (source-space crop + optional scale).
fn build_scene_vf(crop_str: Option<&str>, scale_factor: f64, eff_w: u32, eff_h: u32) -> Option<String> {
    let has_crop  = crop_str.is_some();
    let has_scale = scale_factor < 1.0;

    match (has_crop, has_scale) {
        (false, false) => None,
        (true,  false) => crop_str.map(|s| s.to_owned()),
        (false, true)  => {
            let tw = round_down_even((eff_w as f64 * scale_factor) as u32);
            let th = round_down_even((eff_h as f64 * scale_factor) as u32);
            Some(format!("scale={tw}:{th}"))
        }
        (true,  true)  => {
            let tw = round_down_even((eff_w as f64 * scale_factor) as u32);
            let th = round_down_even((eff_h as f64 * scale_factor) as u32);
            Some(format!("{},scale={tw}:{th}", crop_str.unwrap()))
        }
    }
}

fn round_down_even(v: u32) -> u32 {
    v & !1
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

async fn probe_fps(source: &Path) -> Result<(u32, u32)> {
    #[derive(serde::Deserialize)]
    struct Probe { streams: Vec<Stream> }
    #[derive(serde::Deserialize)]
    struct Stream { avg_frame_rate: String }

    let out = tokio::process::Command::new(external_bin("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0",
               "-show_entries", "stream=avg_frame_rate",
               "-of", "json"])
        .arg(source)
        .output()
        .await
        .context("start ffprobe for fps detection")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffprobe failed:\n{stderr}");
    }

    let p: Probe = serde_json::from_slice(&out.stdout)
        .context("parse ffprobe fps output")?;
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

