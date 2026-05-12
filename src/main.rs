mod audio;
mod config;
mod crop;
mod encode;
mod ffms2;
mod hdr;
mod job;
mod resume;
mod scanner;
mod scene;
mod subtitle;
mod workers;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let input_dir = env_path("AVXS_INPUT_DIR", "/input");
    let output_dir = env_path("AVXS_OUTPUT_DIR", "/output");
    let poll_interval = env_u64("AVXS_POLL_INTERVAL", 60);

    tracing::info!(
        input = %input_dir.display(),
        output = %output_dir.display(),
        poll_s = poll_interval,
        "avxs started"
    );

    ensure_dirs(&input_dir, &output_dir)?;

    let ctx = job::JobContext {
        input_dir: input_dir.clone(),
        output_dir: output_dir.clone(),
    };

    loop {
        match scanner::scan(&input_dir, &output_dir) {
            Err(e) => tracing::error!("scanner error: {e:#}"),
            Ok(jobs) if jobs.is_empty() => {
                tracing::debug!("no jobs — sleeping {poll_interval}s");
            }
            Ok(jobs) => {
                tracing::info!("{} job(s) queued", jobs.len());
                for j in &jobs {
                    let stem = j
                        .source_file
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("video");

                    if let Err(e) = job::run(j, &ctx).await {
                        job::handle_failure(j, &ctx, stem, &e);
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(poll_interval)).await;
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var(var)
        .unwrap_or_else(|_| default.to_string())
        .into()
}

fn env_u64(var: &str, default: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn ensure_dirs(input_dir: &std::path::Path, output_dir: &std::path::Path) -> Result<()> {
    for dir in [input_dir, output_dir] {
        if !dir.exists() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create directory: {}", dir.display()))?;
        }
    }
    Ok(())
}
