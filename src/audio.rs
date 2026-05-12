use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::config::{AudioConfig, AudioMode};

#[derive(Deserialize)]
struct FfprobeOutput {
    streams: Vec<FfprobeStream>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    codec_name: String,
    #[serde(default)]
    tags: FfprobeTags,
}

#[derive(Deserialize, Default)]
struct FfprobeTags {
    language: Option<String>,
}

struct AudioTrack {
    audio_index: usize,
    codec_name: String,
    language: Option<String>,
}

#[derive(Deserialize, Default)]
struct FfprobeDisposition {
    #[serde(default)] default: i32,
    #[serde(default)] forced: i32,
    #[serde(default)] hearing_impaired: i32,
    #[serde(default)] visual_impaired: i32,
    #[serde(default)] original: i32,
    #[serde(default)] comment: i32,
    #[serde(default)] dub: i32,
}

impl FfprobeDisposition {
    fn to_ffmpeg_flags(&self) -> String {
        let flags: Vec<&str> = [
            (self.default != 0,          "default"),
            (self.forced != 0,           "forced"),
            (self.hearing_impaired != 0, "hearing_impaired"),
            (self.visual_impaired != 0,  "visual_impaired"),
            (self.original != 0,         "original"),
            (self.comment != 0,          "comment"),
            (self.dub != 0,              "dub"),
        ]
        .iter()
        .filter_map(|(set, name)| if *set { Some(*name) } else { None })
        .collect();
        if flags.is_empty() { "0".to_string() } else { flags.join("+") }
    }
}

#[derive(Deserialize)]
struct FfprobeDispStream {
    #[serde(default)]
    disposition: FfprobeDisposition,
}

#[derive(Deserialize)]
struct FfprobeDispOutput {
    #[serde(default)]
    streams: Vec<FfprobeDispStream>,
}

async fn probe_dispositions(path: &Path, stream_spec: &str) -> Vec<FfprobeDisposition> {
    let Ok(out) = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", stream_spec,
               "-show_entries", "stream=disposition",
               "-of", "json"])
        .arg(path)
        .output()
        .await
    else { return vec![] };
    serde_json::from_slice::<FfprobeDispOutput>(&out.stdout)
        .map(|p| p.streams.into_iter().map(|s| s.disposition).collect())
        .unwrap_or_default()
}

async fn probe_audio_tracks(source_file: &Path) -> Result<Vec<AudioTrack>> {
    let out = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-select_streams", "a",
            "-show_entries", "stream=codec_name:stream_tags=language",
            "-of", "json",
        ])
        .arg(source_file)
        .output()
        .await
        .context("ffprobe audio streams")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffprobe failed:\n{stderr}");
    }

    let parsed: FfprobeOutput =
        serde_json::from_slice(&out.stdout).context("parse ffprobe output")?;

    Ok(parsed
        .streams
        .into_iter()
        .enumerate()
        .map(|(i, s)| AudioTrack {
            audio_index: i,
            codec_name: s.codec_name,
            language: s.tags.language,
        })
        .collect())
}

fn track_passes_whitelist(track: &AudioTrack, whitelist: &[String]) -> bool {
    if whitelist.is_empty() {
        return true;
    }
    match &track.language {
        // no language tag → always keep
        None => true,
        Some(lang) => whitelist.iter().any(|w| w == lang),
    }
}

pub async fn process(
    source_file: &Path,
    temp_dir: &Path,
    config: &AudioConfig,
) -> Result<PathBuf> {
    let audio_path = temp_dir.join("audio.mkv");

    let tracks = probe_audio_tracks(source_file).await?;

    if tracks.is_empty() {
        return Ok(audio_path);
    }

    let kept: Vec<&AudioTrack> = tracks
        .iter()
        .filter(|t| track_passes_whitelist(t, &config.language_whitelist))
        .collect();

    if kept.is_empty() {
        tracing::warn!(
            "no audio tracks match language whitelist {:?} — audio omitted",
            config.language_whitelist
        );
        return Ok(audio_path);
    }

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(source_file)
        .args(["-vn", "-sn"]);

    for track in &kept {
        cmd.args(["-map", &format!("0:a:{}", track.audio_index)]);
    }

    for (out_idx, track) in kept.iter().enumerate() {
        let (mode, codec, bitrate) = if let Some(rule) = config.codec_rules.get(&track.codec_name) {
            (rule.mode, rule.codec.as_deref(), rule.bitrate.as_deref())
        } else {
            (config.mode, config.codec.as_deref(), config.bitrate.as_deref())
        };

        match mode {
            AudioMode::Copy => {
                cmd.args([format!("-c:a:{out_idx}"), "copy".into()]);
            }
            AudioMode::Encode => {
                let codec = codec.unwrap_or("libopus");
                let bitrate = bitrate.unwrap_or("192k");
                cmd.args([format!("-c:a:{out_idx}"), codec.into()]);
                cmd.args([format!("-b:a:{out_idx}"), bitrate.into()]);
            }
        }
    }

    cmd.arg(&audio_path);

    let out = cmd.output().await.context("start ffmpeg audio extraction")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg audio extraction failed:\n{stderr}");
    }

    Ok(audio_path)
}

pub async fn mux_final(
    video_path: &Path,
    audio_path: &Path,
    source_file: &Path,
    output_path: &Path,
    subtitle: &crate::subtitle::SubtitleSelection,
) -> Result<()> {
    let has_audio = audio_path.exists()
        && std::fs::metadata(audio_path).map(|m| m.len()).unwrap_or(0) > 0;

    // Probe dispositions before building command:
    // - video: from source (encoded video has none)
    // - audio: from audio.mkv (preserved from source even through re-encoding)
    // - subtitle: ffmpeg copies directly from source via -map, no explicit handling needed
    let video_disps = probe_dispositions(source_file, "v").await;
    let audio_disps = if has_audio {
        probe_dispositions(audio_path, "a").await
    } else {
        vec![]
    };

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(video_path);

    if has_audio {
        cmd.arg("-i").arg(audio_path);
    }

    cmd.arg("-i").arg(source_file);

    // Input index of the source file (used for subtitle/chapter mapping)
    let src = if has_audio { 2usize } else { 1usize };

    cmd.args(["-map", "0:v:0"]);

    if has_audio {
        cmd.args(["-map", "1:a?"]);
    }

    match subtitle {
        crate::subtitle::SubtitleSelection::Strip => {}
        crate::subtitle::SubtitleSelection::All => {
            cmd.args(["-map", &format!("{src}:s?")]);
        }
        crate::subtitle::SubtitleSelection::Indices(indices) => {
            for idx in indices {
                cmd.args(["-map", &format!("{src}:s:{idx}")]);
            }
        }
    }

    // Always carry over chapters
    cmd.args(["-map", &format!("{src}:t?")]);

    // Apply dispositions (default, forced, hearing_impaired, …) from source
    for (i, d) in video_disps.iter().enumerate() {
        cmd.args([format!("-disposition:v:{i}"), d.to_ffmpeg_flags()]);
    }
    for (i, d) in audio_disps.iter().enumerate() {
        cmd.args([format!("-disposition:a:{i}"), d.to_ffmpeg_flags()]);
    }

    cmd.args(["-c", "copy"])
        .args(["-map_metadata:g", "-1"])
        .arg(output_path);

    let out = cmd.output().await.context("start ffmpeg mux")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg mux failed:\n{stderr}");
    }

    // Strip all MKV tags (global + per-track) — ffmpeg carries them over from source
    let tag_result = Command::new("mkvpropedit")
        .args(["--tags", "all:"])
        .arg(output_path)
        .output()
        .await;
    match tag_result {
        Ok(o) if !o.status.success() => {
            tracing::warn!("mkvpropedit tag strip: {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(e) => tracing::warn!("mkvpropedit not available: {e}"),
        _ => {}
    }

    Ok(())
}
