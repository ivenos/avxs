use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command;

use crate::config::{SubtitleConfig, SubtitleMode};
use crate::paths::external_bin;

pub enum SubtitleSelection {
    Strip,
    All,
    Indices(Vec<usize>),
}

pub async fn select_tracks(source: &Path, config: &SubtitleConfig) -> Result<SubtitleSelection> {
    if config.mode == SubtitleMode::Strip {
        return Ok(SubtitleSelection::Strip);
    }
    if config.language_whitelist.is_empty() {
        return Ok(SubtitleSelection::All);
    }

    let stdout = probe_subtitle_langs(source).await?;

    #[derive(Deserialize)]
    struct Probe { streams: Vec<Stream> }
    #[derive(Deserialize)]
    struct Stream { #[serde(default)] tags: Tags }
    #[derive(Deserialize, Default)]
    struct Tags { language: Option<String> }

    let probe: Probe = serde_json::from_slice(&stdout)
        .context("parse ffprobe subtitle output")?;

    let indices: Vec<usize> = probe.streams.iter().enumerate()
        .filter(|(_, s)| match &s.tags.language {
            None       => true,
            Some(lang) => config.language_whitelist.iter().any(|w| w == lang),
        })
        .map(|(i, _)| i)
        .collect();

    Ok(SubtitleSelection::Indices(indices))
}

/// ffprobe subtitle languages as JSON, retried since the probe can fail transiently.
async fn probe_subtitle_langs(source: &Path) -> Result<Vec<u8>> {
    let mut last = String::new();
    for attempt in 1..=3 {
        let out = Command::new(external_bin("ffprobe"))
            .args([
                "-v", "error",
                "-select_streams", "s",
                "-show_entries", "stream_tags=language",
                "-of", "json",
            ])
            .arg(source)
            .output()
            .await
            .context("ffprobe subtitle streams")?;

        if out.status.success() {
            return Ok(out.stdout);
        }
        last = String::from_utf8_lossy(&out.stderr).trim().to_string();
        tracing::warn!(
            "ffprobe subtitle probe attempt {attempt}/3 failed (exit {:?}): {last}",
            out.status.code()
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    bail!("ffprobe subtitle probe failed after 3 attempts: {last}");
}

pub(crate) async fn probe_track_ids(source: &Path, subtitle_indices: &[usize]) -> Result<Vec<u64>> {
    let out = Command::new(external_bin("mkvmerge"))
        .args(["--identify", "--identification-format", "json"])
        .arg(source)
        .output()
        .await
        .context("mkvmerge --identify")?;

    if out.status.code().unwrap_or(2) >= 2 {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("mkvmerge identify failed:\n{stderr}");
    }

    #[derive(Deserialize)]
    struct Identify { tracks: Vec<Track> }
    #[derive(Deserialize)]
    struct Track { id: u64, #[serde(rename = "type")] track_type: String }

    let identified: Identify = serde_json::from_slice(&out.stdout)
        .context("parse mkvmerge identify output")?;

    let subtitle_ids: Vec<u64> = identified.tracks.iter()
        .filter(|t| t.track_type == "subtitles")
        .map(|t| t.id)
        .collect();

    Ok(subtitle_indices.iter()
        .filter_map(|&i| match subtitle_ids.get(i) {
            Some(&id) => Some(id),
            None => {
                tracing::warn!("subtitle index {i} out of range ({} tracks) - skipped", subtitle_ids.len());
                None
            }
        })
        .collect())
}
