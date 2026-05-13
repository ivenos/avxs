use anyhow::{bail, Context, Result};
use av_scenechange::{DetectionOptions, SceneDetectionSpeed, av_decoders};
use std::io::{BufReader, Read};
use std::path::Path;
use std::process::Stdio;

use crate::config::{SceneDetectionConfig, SceneDetectionSpeedConfig};
use crate::resume::SceneEntry;

pub async fn detect(
    source_file: &Path,
    cfg: &SceneDetectionConfig,
    vf_filter: Option<&str>,
    fps: f64,
) -> Result<Vec<SceneEntry>> {
    let source = source_file.to_owned();
    let cfg = cfg.clone();
    let vf_filter = vf_filter.map(|s| s.to_owned());

    let result = tokio::task::spawn_blocking(move || {
        run_detection(&source, &cfg, vf_filter.as_deref(), fps)
    })
    .await
    .context("spawn_blocking scene detection")??;

    Ok(result)
}

fn run_detection(
    source_file: &Path,
    cfg: &SceneDetectionConfig,
    vf_filter: Option<&str>,
    fps: f64,
) -> Result<Vec<SceneEntry>> {
    let actual_vf = build_detection_vf(vf_filter, cfg.downscale_height);

    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(source_file);

    if let Some(ref vf) = actual_vf {
        cmd.args(["-vf", vf]);
    }

    let mut ffmpeg = cmd
        .args(["-pix_fmt", "yuv420p"])
        .args(["-f", "yuv4mpegpipe", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start ffmpeg for scene detection")?;

    let stdout = ffmpeg.stdout.take().expect("ffmpeg stdout unavailable");
    // Drain stderr on a background thread to prevent the pipe buffer from filling
    // up and blocking ffmpeg while we are busy consuming stdout.
    let stderr_handle = {
        let stderr = ffmpeg.stderr.take().expect("ffmpeg stderr unavailable");
        std::thread::spawn(move || {
            let mut buf = String::new();
            BufReader::new(stderr).read_to_string(&mut buf).ok();
            buf
        })
    };
    let reader: Box<dyn Read> = Box::new(BufReader::new(stdout));

    let y4m_dec = match y4m::decode(reader).context("init y4m decoder") {
        Ok(d) => d,
        Err(e) => { let _ = ffmpeg.kill(); let _ = ffmpeg.wait(); return Err(e); }
    };
    let decoder_impl = av_decoders::DecoderImpl::Y4m(y4m_dec);
    let mut decoder = match av_decoders::Decoder::from_decoder_impl(decoder_impl)
        .context("create decoder")
    {
        Ok(d) => d,
        Err(e) => { let _ = ffmpeg.kill(); let _ = ffmpeg.wait(); return Err(e); }
    };

    let speed = match cfg.speed {
        SceneDetectionSpeedConfig::Standard => SceneDetectionSpeed::Standard,
        SceneDetectionSpeedConfig::Fast     => SceneDetectionSpeed::Fast,
    };

    let opts = DetectionOptions {
        analysis_speed: speed,
        detect_flashes: true,
        min_scenecut_distance: Some(cfg.min_scene_len),
        max_scenecut_distance: None,
        lookahead_distance: 5,
    };

    let results = av_scenechange::detect_scene_changes::<u8>(&mut decoder, opts, None, None);
    let _ = ffmpeg.kill();
    let _ = ffmpeg.wait();
    let ffmpeg_stderr = stderr_handle.join().unwrap_or_default();
    if !ffmpeg_stderr.is_empty() {
        tracing::warn!("ffmpeg scene detection: {}", ffmpeg_stderr.trim());
    }
    let results = results.context("av-scenechange failed")?;

    if results.frame_count == 0 {
        bail!("scene detection: no frames processed");
    }

    let scenes = build_scene_entries(&results.scene_changes, results.frame_count);

    Ok(match cfg.effective_extra_split_frames(fps) {
        Some(max) => apply_extra_split(scenes, max),
        None      => scenes,
    })
}

fn build_detection_vf(base_vf: Option<&str>, downscale_height: Option<u32>) -> Option<String> {
    match (base_vf, downscale_height) {
        (None,    None)    => None,
        (Some(v), None)    => Some(v.to_owned()),
        (None,    Some(h)) => Some(format!("scale=-2:min(ih\\,{h})")),
        (Some(v), Some(h)) => Some(format!("{v},scale=-2:min(ih\\,{h})")),
    }
}

fn apply_extra_split(scenes: Vec<SceneEntry>, max_frames: usize) -> Vec<SceneEntry> {
    let mut result = Vec::new();
    let mut index = 0usize;

    for scene in scenes {
        let len = (scene.end_frame - scene.start_frame + 1) as usize;
        if len <= max_frames {
            result.push(SceneEntry { index, start_frame: scene.start_frame, end_frame: scene.end_frame });
            index += 1;
        } else {
            let n_parts = len.div_ceil(max_frames);
            let part_size = len / n_parts;
            for i in 0..n_parts {
                let start = scene.start_frame + (i * part_size) as u64;
                let end   = if i + 1 == n_parts {
                    scene.end_frame
                } else {
                    start + part_size as u64 - 1
                };
                result.push(SceneEntry { index, start_frame: start, end_frame: end });
                index += 1;
            }
        }
    }

    result
}

fn build_scene_entries(scene_changes: &[usize], total_frames: usize) -> Vec<SceneEntry> {
    let starts: Vec<usize> = std::iter::once(0)
        .chain(scene_changes.iter().copied().filter(|&f| f > 0))
        .collect();

    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts
                .get(i + 1)
                .map(|&s| s - 1)
                .unwrap_or(total_frames - 1);
            SceneEntry {
                index: i,
                start_frame: start as u64,
                end_frame: end as u64,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_entries_single_chunk() {
        let entries = build_scene_entries(&[0], 100);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].start_frame, 0);
        assert_eq!(entries[0].end_frame, 99);
    }

    #[test]
    fn build_entries_two_chunks() {
        let entries = build_scene_entries(&[0, 50], 100);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].start_frame, 0);
        assert_eq!(entries[0].end_frame, 49);
        assert_eq!(entries[1].start_frame, 50);
        assert_eq!(entries[1].end_frame, 99);
    }

    #[test]
    fn build_entries_indices_sequential() {
        let entries = build_scene_entries(&[0, 24, 48, 72], 100);
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.index, i);
        }
    }

    #[test]
    fn detection_vf_none_no_downscale() {
        assert_eq!(build_detection_vf(None, None), None);
    }

    #[test]
    fn detection_vf_base_only() {
        assert_eq!(
            build_detection_vf(Some("crop=1920:800:0:140"), None),
            Some("crop=1920:800:0:140".into())
        );
    }

    #[test]
    fn detection_vf_downscale_only() {
        assert_eq!(
            build_detection_vf(None, Some(720)),
            Some("scale=-2:min(ih\\,720)".into())
        );
    }

    #[test]
    fn detection_vf_combined() {
        assert_eq!(
            build_detection_vf(Some("crop=1920:800:0:140"), Some(720)),
            Some("crop=1920:800:0:140,scale=-2:min(ih\\,720)".into())
        );
    }

    #[test]
    fn extra_split_no_split_needed() {
        let scenes = build_scene_entries(&[0, 100], 200);
        let result = apply_extra_split(scenes, 240);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].end_frame, 99);
        assert_eq!(result[1].start_frame, 100);
    }

    #[test]
    fn extra_split_exact_boundary() {
        let scenes = build_scene_entries(&[0], 240);
        let result = apply_extra_split(scenes, 240);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].end_frame, 239);
    }

    #[test]
    fn extra_split_one_over() {
        let scenes = build_scene_entries(&[0], 241);
        let result = apply_extra_split(scenes, 240);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].end_frame - result[0].start_frame + 1, 120);
        assert_eq!(result[1].end_frame, 240);
    }

    #[test]
    fn extra_split_triple() {
        let scenes = build_scene_entries(&[0], 720);
        let result = apply_extra_split(scenes, 240);
        assert_eq!(result.len(), 3);
        assert_eq!(result[2].end_frame, 719);
        for e in &result { assert!(e.end_frame - e.start_frame + 1 <= 240); }
    }

    #[test]
    fn extra_split_reindexes() {
        let scenes = build_scene_entries(&[0, 50], 200);
        let mut long = scenes;
        long[1].end_frame = 800;
        let result = apply_extra_split(long, 100);
        for (i, e) in result.iter().enumerate() {
            assert_eq!(e.index, i);
        }
    }
}
