use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command;

use crate::config::{SubtitleConfig, SubtitleMode};

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

    let out = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-select_streams", "s",
            "-show_entries", "stream_tags=language",
            "-of", "json",
        ])
        .arg(source)
        .output()
        .await
        .context("ffprobe subtitle streams")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffprobe subtitle probe failed:\n{stderr}");
    }

    #[derive(Deserialize)]
    struct Probe { streams: Vec<Stream> }
    #[derive(Deserialize)]
    struct Stream { #[serde(default)] tags: Tags }
    #[derive(Deserialize, Default)]
    struct Tags { language: Option<String> }

    let probe: Probe = serde_json::from_slice(&out.stdout)
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
