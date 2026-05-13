use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::Semaphore;

use crate::audio;
use crate::config::Config;
use crate::encode::{self, EncodeOptions};
use crate::ffms2::{self, Crop};
use crate::resume::{DoneFile, SceneEntry, TempDir};
use crate::scanner::Job;
use crate::scene;
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

    if temp.failed_path.exists() {
        tracing::warn!("[{stem}] permanently failed — delete .avxs_{stem}/.failed to retry");
        return Ok(());
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
        ffms2::VideoSource::open(&source_path, &index_path).map(|vs| vs.info.clone())
    })
    .await
    .context("spawn_blocking VideoSource")??;

    let num_workers = workers::calculate(&video_info, stem);

    // -----------------------------------------------------------------------
    // FPS (resolved before crop/keyint so duration_secs is valid)
    // -----------------------------------------------------------------------
    let (fps_num, fps_den) = probe_fps(&job.source_file).await
        .context("ffprobe could not determine fps")?;
    let fps = fps_num as f64 / fps_den as f64;

    // -----------------------------------------------------------------------
    // Auto-HDR
    // -----------------------------------------------------------------------
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

    // -----------------------------------------------------------------------
    // Auto-Crop
    // -----------------------------------------------------------------------
    let crop_str: Option<String> = if config.avxs.crop {
        let duration_secs = video_info.num_frames as f64 / fps;
        crate::crop::detect(&job.source_file, duration_secs, &temp.crop_cache, stem).await?
    } else {
        None
    };

    // -----------------------------------------------------------------------
    // Auto-Scale + derive FFMS2 target dimensions and scaled crop
    // -----------------------------------------------------------------------
    let (ffms2_target, scaled_crop, scene_vf) = compute_output_params(
        video_info.width,
        video_info.height,
        crop_str.as_deref(),
        config.avxs.scale,
        stem,
    );

    // -----------------------------------------------------------------------
    // Auto-Keyint  (~5 s keyframe distance from source FPS)
    // -----------------------------------------------------------------------
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
        ffms2_target,
        crop: scaled_crop,
        fps_num,
        fps_den,
    });

    // -----------------------------------------------------------------------
    // Scene detection
    // -----------------------------------------------------------------------
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

    let total_chunks = scenes.len();
    let total_frames: u64 = scenes.iter().map(|s| s.frame_count()).sum();

    {
        let mut args = config.encoder_args();
        for pair in encode_opts.hdr_args.chunks(2) {
            if let [flag, value] = pair {
                if !config.encoder_params.contains_key(flag.trim_start_matches('-')) {
                    args.push(flag.clone());
                    args.push(value.clone());
                }
            }
        }
        if let Some(ki) = encode_opts.keyint {
            if !config.encoder_params.contains_key("keyint") {
                args.extend_from_slice(&["--keyint".into(), ki.to_string()]);
            }
        }
        // Pair up --key value into "key=value" for a compact single-line summary
        let summary: Vec<String> = args.chunks(2)
            .filter_map(|pair| match pair {
                [k, v] => Some(format!("{}={}", k.trim_start_matches('-'), v)),
                _      => None,
            })
            .collect();
        tracing::info!("[{stem}] encoder args: {}", summary.join(" "));
    }

    tracing::info!("[{stem}] encoding: {total_chunks} chunks, {num_workers} worker(s)");

    // -----------------------------------------------------------------------
    // Parallel chunk encoding
    // -----------------------------------------------------------------------
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

        set.spawn(async move {
            let _permit = sem.acquire().await.expect("semaphore closed");

            let scene_frames = scene.frame_count();
            let t0           = std::time::Instant::now();
            let size_bytes   = tokio::task::spawn_blocking(move || {
                encode::encode_chunk(source, index, scene, chunk_path, &cfg, &opts)
            })
            .await
            .context("spawn_blocking encode_chunk")??;

            let enc_fps = scene_frames as f64 / t0.elapsed().as_secs_f64();

            done.mark_done(&chunk_key, scene_frames, size_bytes).await?;

            let n_chunks = completed_chunks.fetch_add(1, Ordering::Relaxed) + 1;
            let n_frames = completed_frames.fetch_add(scene_frames, Ordering::Relaxed) + scene_frames;
            let pct      = n_frames * 100 / total_f;
            tracing::info!(
                "[{stem_owned}] chunk {n_chunks}/{total_c} — {pct}% — {enc_fps:.1} fps — {:.1} MB",
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
    let audio_path = audio::process(&job.source_file, &temp.path, &config.audio).await?;

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
    tracing::error!("[{stem}] job failed — source kept, temp dir preserved\n{err:#}");

    let temp = TempDir::for_video(&ctx.output_dir, stem);
    if let Err(e) = std::fs::write(&temp.failed_path, format!("{err:#}")) {
        tracing::warn!("[{stem}] could not write failure marker: {e}");
    }
}

// ---------------------------------------------------------------------------
// Output parameter calculation
// ---------------------------------------------------------------------------
//
// Returns:
//   (ffms2_target, scaled_crop, scene_vf_filter)
//
// ffms2_target  — dimensions FFMS2 should scale the full frame to (None = native)
// scaled_crop   — crop region within the FFMS2 output (coordinates already scaled)
// scene_vf      — ffmpeg -vf string for scene detection (crop and/or scale combined)
//
// Logic:
//   1. Parse source-space crop (from cropdetect output)
//   2. Determine effective source dimensions after crop
//   3. If scale target set and effective height > target: calculate scale factor
//   4. When scaling: FFMS2 scales the full source frame, crop coordinates are
//      mapped proportionally into the scaled space
//   5. scene_vf uses ffmpeg crop/scale filters (source-space coordinates, so no
//      remapping needed there)

fn compute_output_params(
    src_w: u32,
    src_h: u32,
    crop_str: Option<&str>,
    target_height: Option<u32>,
    stem: &str,
) -> (Option<(u32, u32)>, Option<Crop>, Option<String>) {
    let src_crop = crop_str.and_then(Crop::from_str);

    let (eff_w, eff_h, eff_x, eff_y) = match src_crop {
        Some(c) => (c.w, c.h, c.x, c.y),
        None    => (src_w, src_h, 0, 0),
    };

    let scale_factor: f64 = match target_height {
        Some(th) if eff_h > th => th as f64 / eff_h as f64,
        _ => 1.0,
    };

    if scale_factor < 1.0 {
        tracing::info!(
            "[{stem}] auto-scale: {eff_w}×{eff_h} → {}×{} (factor {scale_factor:.4})",
            round_down_even((eff_w as f64 * scale_factor) as u32),
            round_down_even((eff_h as f64 * scale_factor) as u32),
        );
    }

    // scene detection vf filter (uses source-space coordinates directly)
    let scene_vf: Option<String> = build_scene_vf(crop_str, scale_factor, eff_w, eff_h);

    if scale_factor == 1.0 {
        // No scaling — return src_crop unchanged for the FFMS2 pipeline
        return (None, src_crop, scene_vf);
    }

    // Scaling active: FFMS2 scales the full source frame
    let ffms2_w = round_down_even((src_w as f64 * scale_factor) as u32);
    let ffms2_h = round_down_even((src_h as f64 * scale_factor) as u32);

    // Map crop into scaled space (only when original had black bars)
    let scaled_crop = if src_crop.is_some() {
        Some(Crop {
            w: round_down_even((eff_w as f64 * scale_factor) as u32),
            h: round_down_even((eff_h as f64 * scale_factor) as u32),
            x: round_down_even((eff_x as f64 * scale_factor) as u32),
            y: round_down_even((eff_y as f64 * scale_factor) as u32),
        })
    } else {
        None
    };

    (Some((ffms2_w, ffms2_h)), scaled_crop, scene_vf)
}

/// Build the ffmpeg -vf filter string for scene detection.
/// Uses source-space crop coordinates — no scaling needed since ffmpeg handles both in one filter.
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

// ---------------------------------------------------------------------------
// File stability check
// ---------------------------------------------------------------------------

/// Waits until the file size is stable (two consecutive checks are equal).
/// Uses a short 200 ms initial check so already-complete files return immediately.
/// Falls back to 2 s polling when the file is actively being written. Times out after 300 s.
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

    tracing::info!("[{stem}] file is still being written — waiting...");
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

async fn probe_fps(source: &Path) -> Option<(u32, u32)> {
    #[derive(serde::Deserialize)]
    struct Probe { streams: Vec<Stream> }
    #[derive(serde::Deserialize)]
    struct Stream { avg_frame_rate: String }

    let out = tokio::process::Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0",
               "-show_entries", "stream=avg_frame_rate",
               "-of", "json"])
        .arg(source)
        .output()
        .await
        .ok()?;

    let p: Probe = serde_json::from_slice(&out.stdout).ok()?;
    let rate = p.streams.into_iter().next()?.avg_frame_rate;
    if let Some((n, d)) = rate.split_once('/') {
        let n: u32 = n.trim().parse().ok()?;
        let d: u32 = d.trim().parse().ok()?;
        if d > 0 && n > 0 { Some((n, d)) } else { None }
    } else {
        let n: u32 = rate.trim().parse().ok()?;
        if n > 0 { Some((n, 1)) } else { None }
    }
}

