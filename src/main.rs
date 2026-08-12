mod audio;
mod config;
mod crop;
mod encode;
mod ext;
mod ffms2;
mod hdr;
mod job;
mod resume;
mod scanner;
mod scene;
mod subtitle;
mod target_quality;
mod workers;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let input_dir = env_path("AVXS_INPUT_DIR", "./input");
    let output_dir = env_path("AVXS_OUTPUT_DIR", "./output");
    let poll_interval = env_u64("AVXS_POLL_INTERVAL", 60).max(1);

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

    let mut shutdown = shutdown_signal();

    loop {
        let in_dir  = input_dir.clone();
        let out_dir = output_dir.clone();
        let scan_result = tokio::task::spawn_blocking(move || scanner::scan(&in_dir, &out_dir))
            .await
            .context("spawn_blocking scanner")?;

        match scan_result {
            Err(e) => tracing::error!("scanner error: {e:#}"),
            Ok(jobs) if jobs.is_empty() => {
                tracing::debug!("no jobs - sleeping {poll_interval}s");
            }
            Ok(jobs) => {
                tracing::info!("{} job(s) queued", jobs.len());
                for j in &jobs {
                    let stem = j.stem();

                    if let Err(e) = job::run(j, &ctx).await {
                        job::handle_failure(j, &ctx, stem, &e);
                    }
                    if *shutdown.borrow() {
                        tracing::info!("stopping after {stem}");
                        return Ok(());
                    }
                }
            }
        }

        if *shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(poll_interval)) => {}
            // Ok only: `changed()` also resolves once the sender is gone.
            Ok(()) = shutdown.changed() => return Ok(()),
        }
    }
}

/// Ends the scan loop between jobs. Killed instead, the orphaned encoders keep writing
/// chunk files the restarted instance starts writing too.
fn shutdown_signal() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);

    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let (Ok(mut term), Ok(mut int)) =
                (signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
            else {
                tracing::error!("could not install signal handlers - avxs will keep running on SIGTERM until it is killed");
                return;
            };
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::info!("signal received - finishing the current job, then stopping");
        let _ = tx.send(true);
    });

    rx
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
    match std::env::var(var) {
        Err(_) => default,
        Ok(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => {
                tracing::warn!("{var} has invalid value {v:?} - using default {default}");
                default
            }
        },
    }
}

fn ensure_dirs(input_dir: &std::path::Path, output_dir: &std::path::Path) -> Result<()> {
    for dir in [input_dir, output_dir] {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create directory: {}", dir.display()))?;
    }
    Ok(())
}
