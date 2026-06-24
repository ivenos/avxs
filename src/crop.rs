use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

use crate::ext::external_bin;

/// Detects black bars via ffmpeg cropdetect. Result is cached next to the source file.
/// Returns the crop string in ffmpeg format: "crop=W:H:X:Y", or None if no crop needed.
pub async fn detect(
    source_file: &Path,
    duration_secs: f64,
    cache_path: &Path,
    stem: &str,
) -> Result<Option<String>> {
    if cache_path.exists() {
        let cached = std::fs::read_to_string(cache_path)
            .unwrap_or_default()
            .trim()
            .to_string();
        if cached.is_empty() {
            tracing::info!("[{stem}] auto-crop: no black bars (cached)");
        } else {
            tracing::info!("[{stem}] auto-crop: {cached} (cached)");
        }
        return Ok(if cached.is_empty() { None } else { Some(cached) });
    }

    let (orig_w, orig_h) = probe_dimensions(source_file).await?;
    if orig_w == 0 || orig_h == 0 {
        cache_result(cache_path, "");
        return Ok(None);
    }

    tracing::info!("[{stem}] auto-crop: running cropdetect...");

    let source = source_file.to_owned();
    let handles: Vec<_> = [10u64, 25, 40, 55, 70]
        .iter()
        .map(|&pct| {
            let src = source.clone();
            let seek = (duration_secs * pct as f64 / 100.0) as u64;
            tokio::spawn(async move { run_cropdetect(&src, seek).await })
        })
        .collect();

    let mut all_values: Vec<String> = Vec::new();
    for handle in handles {
        all_values.extend(handle.await.unwrap_or_default());
    }

    let result = mode_value(&all_values).and_then(|crop| {
        let inner = crop.trim_start_matches("crop=");
        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() < 2 {
            return None;
        }
        let cw: u64 = parts[0].parse().ok()?;
        let ch: u64 = parts[1].parse().ok()?;
        // Only apply if more than 1% of pixels are cropped away
        if cw * ch < orig_w * orig_h * 99 / 100 {
            Some(crop)
        } else {
            None
        }
    });

    cache_result(cache_path, result.as_deref().unwrap_or(""));

    match &result {
        Some(c) => tracing::info!("[{stem}] auto-crop: detected {c}"),
        None    => tracing::info!("[{stem}] auto-crop: no black bars detected"),
    }

    Ok(result)
}

async fn probe_dimensions(source_file: &Path) -> Result<(u64, u64)> {
    #[derive(serde::Deserialize)]
    struct Root { streams: Vec<Stream> }
    #[derive(serde::Deserialize)]
    struct Stream { width: u64, height: u64 }

    let root: Root = crate::ext::ffprobe_json(
        &["-v", "error", "-select_streams", "v:0",
          "-show_entries", "stream=width,height", "-of", "json"],
        source_file,
    )
    .await
    .unwrap_or(Root { streams: vec![] });
    Ok(root.streams.into_iter().next().map(|s| (s.width, s.height)).unwrap_or((0, 0)))
}

async fn run_cropdetect(source_file: &Path, seek_secs: u64) -> Vec<String> {
    let out = tokio::process::Command::new(external_bin("ffmpeg"))
        .args(["-ss", &seek_secs.to_string()])
        .arg("-i").arg(source_file)
        // threshold=128 for HDR/10-bit sources; round=16 ensures 16-pixel-aligned results
        .args(["-t", "10", "-vf", "cropdetect=128:16:0", "-f", "null", "-"])
        .output()
        .await;

    let output = match out {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    // cropdetect writes results to stderr
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter_map(|line| {
            line.find("crop=").map(|pos| {
                line[pos..].split_whitespace().next().unwrap_or("").to_string()
            })
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn mode_value(values: &[String]) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for v in values {
        *counts.entry(v.as_str()).or_insert(0) += 1;
    }
    // Stable tie-break: prefer the lexicographically smaller crop string (larger crop area wins
    // by count; on equal count, the smaller string is a deterministic fallback).
    counts.into_iter()
        .max_by(|(k1, c1), (k2, c2)| c1.cmp(c2).then_with(|| k2.cmp(k1)))
        .map(|(v, _)| v.to_string())
}

fn cache_result(path: &Path, content: &str) {
    if let Err(e) = std::fs::write(path, content) {
        tracing::warn!("could not write crop cache {}: {e:#}", path.display());
    }
}
