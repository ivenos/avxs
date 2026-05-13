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
}

impl FfprobeDisposition {
    fn to_mkvmerge_flags(&self, tid: usize) -> Vec<String> {
        let t = tid.to_string();
        let yn = |v: i32| if v != 0 { "yes" } else { "no" };
        let mut flags = vec![
            "--default-track-flag".into(), format!("{t}:{}", yn(self.default)),
        ];
        let extras: &[(&str, i32)] = &[
            ("--forced-display-flag",    self.forced),
            ("--hearing-impaired-flag",  self.hearing_impaired),
            ("--visual-impaired-flag",   self.visual_impaired),
            ("--original-flag",          self.original),
            ("--commentary-flag",        self.comment),
        ];
        for (flag, val) in extras {
            if *val != 0 {
                flags.extend([(*flag).to_string(), format!("{t}:yes")]);
            }
        }
        flags
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
    else {
        tracing::warn!("ffprobe disposition probe failed for {}", path.display());
        return vec![];
    };
    serde_json::from_slice::<FfprobeDispOutput>(&out.stdout)
        .map(|p| p.streams.into_iter().map(|s| s.disposition).collect())
        .unwrap_or_else(|e| {
            tracing::warn!("failed to parse ffprobe disposition output for {}: {e}", path.display());
            vec![]
        })
}

async fn probe_audio_tracks(source_file: &Path) -> Result<Vec<AudioTrack>> {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
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
                let codec = codec.ok_or_else(|| anyhow::anyhow!(
                    "audio track {out_idx}: codec is required when mode = encode"
                ))?;
                let bitrate = bitrate.ok_or_else(|| anyhow::anyhow!(
                    "audio track {out_idx}: bitrate is required when mode = encode"
                ))?;
                cmd.args([
                    format!("-filter:a:{out_idx}"),
                    "aformat=channel_layouts=7.1|6.1|5.1|5.1(side)|5.0|quad|stereo|mono".into(),
                ]);
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

    // Video dispositions come from source — the encoded file has none.
    // Audio dispositions are already preserved in audio.mkv by the extraction step.
    let video_disps = probe_dispositions(source_file, "v").await;

    let mut cmd = Command::new("mkvmerge");
    cmd.arg("-o").arg(output_path);

    // Video track: apply dispositions from source, strip everything else
    if let Some(d) = video_disps.first() {
        cmd.args(d.to_mkvmerge_flags(0));
    }
    cmd.args(["--no-audio", "--no-subtitles", "--no-chapters",
              "--no-global-tags", "--no-track-tags"]);
    cmd.arg(video_path);

    // Audio: dispositions already in audio.mkv
    if has_audio {
        cmd.args(["--no-video", "--no-subtitles", "--no-chapters",
                  "--no-global-tags", "--no-track-tags"]);
        cmd.arg(audio_path);
    }

    // Source: subtitles + chapters only, no tags
    cmd.args(["--no-video", "--no-audio", "--no-global-tags", "--no-track-tags"]);
    match subtitle {
        crate::subtitle::SubtitleSelection::Strip => {
            cmd.arg("--no-subtitles");
        }
        crate::subtitle::SubtitleSelection::All => {}
        crate::subtitle::SubtitleSelection::Indices(indices) => {
            if indices.is_empty() {
                cmd.arg("--no-subtitles");
            } else {
                let track_ids = crate::subtitle::probe_track_ids(source_file, indices).await?;
                if track_ids.is_empty() {
                    cmd.arg("--no-subtitles");
                } else {
                    let tracks = track_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
                    cmd.args(["--subtitle-tracks", &tracks]);
                }
            }
        }
    }
    cmd.arg(source_file);

    let out = cmd.output().await.context("start mkvmerge")?;
    // mkvmerge exits 1 for warnings (non-fatal), 2+ for errors
    if out.status.code().unwrap_or(2) >= 2 {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("mkvmerge failed:\n{stderr}");
    }

    Ok(())
}
