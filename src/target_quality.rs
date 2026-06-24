use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::{Config, TargetQualityConfig};
use crate::encode::{self, EncodeOptions};
use crate::ext::external_bin;
use crate::ffms2::{Crop, OpenOpts, VideoSource};
use crate::resume::SceneEntry;

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

pub struct ProbeContext<'a> {
    pub source: &'a Path,
    pub index: &'a Path,
    pub temp_dir: &'a Path,
    pub config: &'a Config,
    pub opts: &'a EncodeOptions,
    pub tq: &'a TargetQualityConfig,
    pub model: &'a str,
    pub n_threads: usize,
}

/// Probes a chunk at several CRFs and returns the one that best hits the VMAF target.
pub fn solve_chunk_crf(ctx: &ProbeContext, scene: &SceneEntry) -> Result<u32> {
    let lo = ctx.tq.min_crf;
    let hi = ctx.tq.max_crf;
    let target = ctx.tq.vmaf;

    let mut points: Vec<(u32, f64)> = Vec::new();
    let mut crf = seed_crf(ctx.config, lo, hi);

    for _ in 0..ctx.tq.probes {
        let v = probe_once(ctx, scene, crf)?;
        tracing::debug!("chunk {} probe crf {crf} gives vmaf {v:.2}", scene.padded_index());
        points.push((crf, v));
        if v >= target - ctx.tq.tolerance_under && v <= target + ctx.tq.tolerance_over {
            return Ok(crf);
        }
        let next = interpolate(&points, target, lo, hi);
        if next == crf {
            break;
        }
        crf = next;
    }
    Ok(pick_best(&points, target, ctx.tq.tolerance_under, lo))
}

fn seed_crf(config: &Config, lo: u32, hi: u32) -> u32 {
    config
        .encoder_params
        .get("crf")
        .and_then(|v| match v {
            toml::Value::Integer(i) => u32::try_from(*i).ok(),
            toml::Value::Float(f)   => Some(*f as u32),
            toml::Value::String(s)  => s.parse().ok(),
            _ => None,
        })
        .unwrap_or((lo + hi) / 2)
        .clamp(lo, hi)
}

fn probe_once(ctx: &ProbeContext, scene: &SceneEntry, crf: u32) -> Result<f64> {
    let tag = format!("{}_{crf}", scene.padded_index());
    let probe = ctx.temp_dir.join(format!("probe_{tag}.ivf"));
    encode::encode_chunk(
        ctx.source.to_path_buf(),
        ctx.index.to_path_buf(),
        scene.clone(),
        probe.clone(),
        ctx.config,
        ctx.opts,
        encode::EncodeOverrides { crf: Some(crf), preset: Some(ctx.tq.probe_preset) },
    )
    .with_context(|| format!("probe encode crf {crf}"))?;

    let result = measure(&MeasureOpts {
        distorted: &probe,
        source: ctx.source,
        index: ctx.index,
        work_dir: ctx.temp_dir,
        start: scene.start_frame,
        end: scene.end_frame,
        crop: ctx.opts.crop,
        scale: ctx.opts.scale,
        target_bit_depth: ctx.opts.target_bit_depth,
        fps_num: ctx.opts.fps_num,
        fps_den: ctx.opts.fps_den,
        model: ctx.model,
        n_threads: ctx.n_threads,
        tag: &tag,
    });
    let _ = std::fs::remove_file(&probe);
    result
}

/// Predict the CRF that hits `target`. VMAF falls as CRF rises, so we interpolate
/// on the two points bracketing the target (or the two nearest), then clamp/round.
fn interpolate(points: &[(u32, f64)], target: f64, lo: u32, hi: u32) -> u32 {
    if points.len() == 1 {
        let (c, v) = points[0];
        // nominal slope of ~0.4 VMAF per CRF step
        let delta = (v - target) / 0.4;
        return clamp_round(c as f64 + delta, lo, hi);
    }
    let ((c1, v1), (c2, v2)) = bracket(points, target);
    if (v1 - v2).abs() < 1e-6 {
        return clamp_round((c1 + c2) as f64 / 2.0, lo, hi);
    }
    let slope = (c2 as f64 - c1 as f64) / (v2 - v1);
    clamp_round(c1 as f64 + slope * (target - v1), lo, hi)
}

/// Two points to interpolate between: prefer one at/above and one below the
/// target; otherwise the two closest to it in VMAF.
fn bracket(points: &[(u32, f64)], target: f64) -> ((u32, f64), (u32, f64)) {
    let above = points.iter().filter(|(_, v)| *v >= target).min_by(|x, y| x.1.total_cmp(&y.1));
    let below = points.iter().filter(|(_, v)| *v < target).max_by(|x, y| x.1.total_cmp(&y.1));
    if let (Some(&a), Some(&b)) = (above, below) {
        return (a, b);
    }
    let mut sorted = points.to_vec();
    sorted.sort_by(|x, y| (x.1 - target).abs().total_cmp(&(y.1 - target).abs()));
    (sorted[0], sorted[1])
}

/// Final pick: the most efficient CRF (highest) whose VMAF stays at or above the
/// lower tolerance; if none qualify, the highest-VMAF probe.
fn pick_best(points: &[(u32, f64)], target: f64, tol_under: f64, lo: u32) -> u32 {
    let floor = target - tol_under;
    if let Some(&(c, _)) = points.iter().filter(|(_, v)| *v >= floor).max_by_key(|(c, _)| *c) {
        return c;
    }
    points
        .iter()
        .max_by(|x, y| x.1.total_cmp(&y.1))
        .map(|&(c, _)| c)
        .unwrap_or(lo)
}

fn clamp_round(v: f64, lo: u32, hi: u32) -> u32 {
    (v.round() as i64).clamp(lo as i64, hi as i64) as u32
}

struct MeasureOpts<'a> {
    distorted: &'a Path,
    source: &'a Path,
    index: &'a Path,
    work_dir: &'a Path,
    start: u64,
    end: u64,
    crop: Option<Crop>,
    scale: Option<(u32, u32)>,
    target_bit_depth: Option<u8>,
    fps_num: u32,
    fps_den: u32,
    model: &'a str,
    n_threads: usize,
    /// Unique suffix for the per-measurement fifo/log files.
    tag: &'a str,
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

/// Mean VMAF of `distorted` against source frames `start..=end`, both fed to the
/// `vmaf` tool as 10-bit Y4M over fifos (reference cropped+scaled like the encode).
fn measure(m: &MeasureOpts) -> Result<f64> {
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

    #[test]
    fn interpolate_brackets_target() {
        // vmaf 97 @ crf30, vmaf 93 @ crf40, target 95 gives crf35
        let pts = vec![(30u32, 97.0), (40u32, 93.0)];
        assert_eq!(interpolate(&pts, 95.0, 10, 60), 35);
    }

    #[test]
    fn interpolate_clamps_to_bounds() {
        let pts = vec![(30u32, 99.5), (32u32, 99.0)];
        // target far below measured range pushes toward hi, clamped
        assert_eq!(interpolate(&pts, 80.0, 18, 45), 45);
    }

    #[test]
    fn pick_best_prefers_highest_crf_above_floor() {
        // target 95, tol_under 0.5 gives floor 94.5; crf32@95.2 and crf36@94.6 both ok, pick 36
        let pts = vec![(30u32, 96.0), (32u32, 95.2), (36u32, 94.6), (40u32, 92.0)];
        assert_eq!(pick_best(&pts, 95.0, 0.5, 18), 36);
    }

    #[test]
    fn pick_best_falls_back_to_best_quality() {
        // all below floor, highest vmaf wins (crf 30)
        let pts = vec![(30u32, 90.0), (35u32, 88.0), (40u32, 85.0)];
        assert_eq!(pick_best(&pts, 95.0, 0.5, 18), 30);
    }

    #[test]
    fn seed_uses_encoder_crf_when_present() {
        let mut config = Config {
            encoder: Some(crate::config::Encoder::SvtAv1),
            ..Default::default()
        };
        config.encoder_params.insert("crf".into(), toml::Value::Integer(28));
        assert_eq!(seed_crf(&config, 18, 45), 28);
        // out of range clamps
        config.encoder_params.insert("crf".into(), toml::Value::Integer(60));
        assert_eq!(seed_crf(&config, 18, 45), 45);
        // absent gives midpoint
        config.encoder_params.clear();
        assert_eq!(seed_crf(&config, 18, 44), 31);
    }
}
