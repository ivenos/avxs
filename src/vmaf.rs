use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::ffms2::{Crop, OpenOpts, VideoSource};
use crate::paths::external_bin;

// VMAF v1 models, picked by output height (1080p vs 4K). Built into the bundled libvmaf.
const MODEL_1080: &str = "vmaf_v1.0.16_3d0h";
const MODEL_2160: &str = "vmaf_v1.0.16_1d5h_2160";

/// VMAF v1 model for the output height.
pub fn model_for_height(output_height: u32) -> &'static str {
    if output_height >= 1440 { MODEL_2160 } else { MODEL_1080 }
}

/// Confirms the bundled `vmaf` tool is runnable; called once before probing.
pub async fn ensure_available() -> Result<()> {
    let out = tokio::process::Command::new(external_bin("vmaf"))
        .arg("--version")
        .output()
        .await
        .context("run vmaf --version (is the vmaf tool bundled?)")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!("vmaf tool is not runnable: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

pub struct MeasureOpts<'a> {
    pub distorted: &'a Path,
    pub source: &'a Path,
    pub index: &'a Path,
    pub work_dir: &'a Path,
    pub start: u64,
    pub end: u64,
    pub crop: Option<Crop>,
    pub scale: Option<(u32, u32)>,
    pub target_bit_depth: Option<u8>,
    pub fps_num: u32,
    pub fps_den: u32,
    pub model: &'a str,
    pub n_threads: usize,
    /// Unique suffix for the per-measurement fifo/log files.
    pub tag: &'a str,
}

#[derive(Deserialize)]
struct VmafLog {
    pooled_metrics: Pooled,
}
#[derive(Deserialize)]
struct Pooled {
    vmaf: Stats,
}
#[derive(Deserialize)]
struct Stats {
    mean: f64,
}

/// Removes its paths on drop so fifos/logs never linger after a measurement.
struct Cleanup(Vec<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Mean VMAF of `distorted` against the source frames `start..=end`.
/// ffmpeg decodes the AV1 probe and scales the reference (same crop+scale as the
/// encode); both Y4M streams feed the `vmaf` tool over fifos. 10-bit for VMAF v1.
pub fn measure(m: &MeasureOpts) -> Result<f64> {
    let ref_fifo  = m.work_dir.join(format!("vmaf_ref_{}.y4m", m.tag));
    let dist_fifo = m.work_dir.join(format!("vmaf_dist_{}.y4m", m.tag));
    let json      = m.work_dir.join(format!("vmaf_{}.json", m.tag));
    let _cleanup = Cleanup(vec![ref_fifo.clone(), dist_fifo.clone(), json.clone()]);

    for f in [&ref_fifo, &dist_fifo] {
        let _ = std::fs::remove_file(f);
        make_fifo(f)?;
    }

    // Distorted: decode the AV1 probe to 10-bit Y4M into its fifo.
    let mut dist_ff = std::process::Command::new(external_bin("ffmpeg"))
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(m.distorted)
        .args(["-vf", "format=yuv420p10le", "-strict", "-1", "-f", "yuv4mpegpipe"])
        .arg(&dist_fifo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("start ffmpeg (decode distorted for vmaf)")?;

    // Reference: FFMS2 (cropped) piped through ffmpeg for scale + 10-bit into its fifo.
    let vf = match m.scale {
        Some((w, h)) => format!("scale={w}:{h}:flags=lanczos,format=yuv420p10le"),
        None         => "format=yuv420p10le".to_string(),
    };
    let mut ref_ff = std::process::Command::new(external_bin("ffmpeg"))
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "yuv4mpegpipe", "-i", "pipe:0"])
        .args(["-vf", &vf, "-strict", "-1", "-f", "yuv4mpegpipe"])
        .arg(&ref_fifo)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("start ffmpeg (reference for vmaf)")?;
    let ref_stdin = ref_ff.stdin.take().expect("ref ffmpeg stdin unavailable");

    // FFMS2 pointer is not Send, so open the source inside the writer thread.
    let source = m.source.to_path_buf();
    let index  = m.index.to_path_buf();
    let (start, end) = (m.start, m.end);
    let crop = m.crop;
    let depth = m.target_bit_depth;
    let (fps_num, fps_den) = (m.fps_num, m.fps_den);
    let writer = std::thread::spawn(move || -> Result<()> {
        let mut vs = VideoSource::open(&source, &index, OpenOpts { target_bit_depth: depth })
            .context("open FFMS2 for vmaf reference")?;
        vs.info.fps_num = fps_num;
        vs.info.fps_den = fps_den;
        let mut w = BufWriter::with_capacity(256 * 1024, ref_stdin);
        vs.write_y4m_range(&mut w, start, end, crop).context("write Y4M reference")?;
        Ok(())
    });

    let model_arg = format!("version={}", m.model);
    let out = std::process::Command::new(external_bin("vmaf"))
        .arg("-r").arg(&ref_fifo)
        .arg("-d").arg(&dist_fifo)
        .args(["-m", &model_arg, "--json", "-o"])
        .arg(&json)
        .args(["--threads", &m.n_threads.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("run vmaf")?;

    let _ = dist_ff.wait();
    let _ = ref_ff.wait();
    let writer_res = writer.join().map_err(|_| anyhow!("vmaf reference writer panicked"))?;

    if !out.status.success() {
        bail!("vmaf failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }
    writer_res?;

    let raw = std::fs::read_to_string(&json)
        .with_context(|| format!("read vmaf json: {}", json.display()))?;
    let log: VmafLog = serde_json::from_str(&raw).context("parse vmaf json")?;
    Ok(log.pooled_metrics.vmaf.mean)
}

fn make_fifo(path: &Path) -> Result<()> {
    let status = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .context("run mkfifo")?;
    if !status.success() {
        bail!("mkfifo failed for {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_picked_by_height() {
        assert_eq!(model_for_height(720), MODEL_1080);
        assert_eq!(model_for_height(1080), MODEL_1080);
        assert_eq!(model_for_height(1439), MODEL_1080);
        assert_eq!(model_for_height(1440), MODEL_2160);
        assert_eq!(model_for_height(2160), MODEL_2160);
    }
}
