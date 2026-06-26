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

// CRF granularity of the SVT-AV1 encoders (and the HDR fork): quarter steps.
const CRF_STEP: f64 = 0.25;

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
    pub stem: &'a str,
    /// Cumulative source byte sizes by frame (len = frames + 1); empty disables the cap.
    pub source_byte_index: &'a [u64],
}

#[derive(Clone, Copy)]
struct Probe {
    crf: f64,
    vmaf: f64,
    size_pct: f64,
}

/// Why the search settled on its CRF, for logging.
pub enum SolveOutcome {
    /// Highest CRF that holds the VMAF floor within the size cap.
    Met,
    /// Size cap forced a higher CRF than the floor allowed; VMAF is below target.
    CapBinding,
    /// Floor unreachable in the CRF range; best-quality probe used.
    FloorUnreachable,
}

pub struct SolveResult {
    pub crf: f64,
    pub vmaf: f64,
    pub size_pct: f64,
    pub outcome: SolveOutcome,
}

/// Finds the highest CRF whose VMAF holds the floor `tq.vmaf` and whose size
/// stays under `tq.max_encoded_percent`. VMAF falls monotonically as CRF rises,
/// so this is a threshold search: interpolated binary search on a 0.25 grid.
pub fn solve_chunk_crf(ctx: &ProbeContext, scene: &SceneEntry) -> Result<SolveResult> {
    let lo = ctx.tq.min_crf as f64;
    let hi = ctx.tq.max_crf as f64;
    let target = ctx.tq.vmaf;
    let tol = ctx.tq.tolerance;
    let cap = ctx.tq.max_encoded_percent;
    let key = scene.padded_index();

    let mut pts: Vec<Probe> = Vec::new();
    let mut crf = round_to_step(seed_crf(ctx.config, lo, hi), lo, hi);

    for i in 0..ctx.tq.max_probes {
        let (vmaf, size_pct) = probe_once(ctx, scene, crf)?;
        tracing::info!(
            "[{}] chunk {key} probe {}/{} crf {crf} gives VMAF {vmaf:.2}, {size_pct:.0}% size",
            ctx.stem, i + 1, ctx.tq.max_probes
        );
        pts.push(Probe { crf, vmaf, size_pct });

        // early stop: just above the floor, within the size cap, after min_probes
        if i + 1 >= ctx.tq.min_probes
            && vmaf >= target && vmaf <= target + tol && size_pct <= cap
        {
            break;
        }
        match next_crf(&pts, target, lo, hi) {
            Some(next) if (next - crf).abs() > 1e-9 => crf = next,
            _ => break,
        }
    }

    Ok(decide(&pts, target, cap, lo))
}

fn seed_crf(config: &Config, lo: f64, hi: f64) -> f64 {
    config
        .encoder_params
        .get("crf")
        .and_then(|v| match v {
            toml::Value::Integer(i) => Some(*i as f64),
            toml::Value::Float(f)   => Some(*f),
            toml::Value::String(s)  => s.parse().ok(),
            _ => None,
        })
        .unwrap_or((lo + hi) / 2.0)
        .clamp(lo, hi)
}

/// Encodes the chunk at `crf` (probe preset), measures VMAF, and reports the
/// encoded size relative to the source over the chunk's duration.
fn probe_once(ctx: &ProbeContext, scene: &SceneEntry, crf: f64) -> Result<(f64, f64)> {
    let tag = format!("{}_{crf}", scene.padded_index());
    let probe = ctx.temp_dir.join(format!("probe_{tag}.ivf"));
    let size_bytes = encode::encode_chunk(
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
    let vmaf = result?;
    let size_pct = chunk_size_pct(size_bytes, ctx.source_byte_index, scene.start_frame, scene.end_frame);
    Ok((vmaf, size_pct))
}

/// Encoded size as a percent of the source's actual bytes for this chunk's frames.
/// Returns 0 when the source index is unavailable, so the cap simply never binds.
fn chunk_size_pct(encoded: u64, cum: &[u64], start: u64, end: u64) -> f64 {
    match (cum.get(start as usize), cum.get(end as usize + 1)) {
        (Some(lo), Some(hi)) if hi > lo => encoded as f64 / (hi - lo) as f64 * 100.0,
        _ => 0.0,
    }
}

/// Picks the next CRF to probe, or None when the crossing is bracketed to one
/// 0.25 step, a bound is reached, or no new grid point remains.
fn next_crf(pts: &[Probe], target: f64, lo: f64, hi: f64) -> Option<f64> {
    let pass: Vec<f64> = pts.iter().filter(|p| p.vmaf >= target).map(|p| p.crf).collect();
    let fail: Vec<f64> = pts.iter().filter(|p| p.vmaf <  target).map(|p| p.crf).collect();

    if !pass.is_empty() && !fail.is_empty() {
        let p = pass.iter().copied().fold(f64::MIN, f64::max); // highest CRF still passing
        let f = fail.iter().copied().fold(f64::MAX, f64::min); // lowest CRF failing
        if f - p <= CRF_STEP + 1e-9 {
            return None; // bracketed to adjacent grid steps
        }
        let mut cand = round_to_step(interpolate_crf(pts, target), p, f);
        if cand <= p + 1e-9 || cand >= f - 1e-9 || already(pts, cand) {
            cand = round_to_step((p + f) / 2.0, p, f); // bisection fallback
        }
        if cand <= p + 1e-9 || cand >= f - 1e-9 || already(pts, cand) {
            return None;
        }
        Some(cand)
    } else if !pass.is_empty() {
        // everything passes: compress harder (toward max_crf)
        let hp = pass.iter().copied().fold(f64::MIN, f64::max);
        if hp >= hi - 1e-9 {
            return None;
        }
        let cand = round_to_step(interpolate_crf(pts, target).max(hp + CRF_STEP), hp + CRF_STEP, hi);
        if already(pts, cand) { None } else { Some(cand) }
    } else {
        // everything fails: raise quality (toward min_crf)
        let lf = fail.iter().copied().fold(f64::MAX, f64::min);
        if lf <= lo + 1e-9 {
            return None;
        }
        let cand = round_to_step(interpolate_crf(pts, target).min(lf - CRF_STEP), lo, lf - CRF_STEP);
        if already(pts, cand) { None } else { Some(cand) }
    }
}

/// Linear (secant) estimate of the CRF that yields `target` VMAF.
fn interpolate_crf(pts: &[Probe], target: f64) -> f64 {
    if pts.len() == 1 {
        // nominal slope of ~0.4 VMAF per CRF step
        return pts[0].crf + (pts[0].vmaf - target) / 0.4;
    }
    let (a, b) = bracket_pts(pts, target);
    if (a.1 - b.1).abs() < 1e-6 {
        return (a.0 + b.0) / 2.0;
    }
    let slope = (b.0 - a.0) / (b.1 - a.1);
    a.0 + slope * (target - a.1)
}

/// Two (crf, vmaf) points to interpolate between: one at/above and one below the
/// target if possible, otherwise the two closest in VMAF.
fn bracket_pts(pts: &[Probe], target: f64) -> ((f64, f64), (f64, f64)) {
    let above = pts.iter().filter(|p| p.vmaf >= target).min_by(|x, y| x.vmaf.total_cmp(&y.vmaf));
    let below = pts.iter().filter(|p| p.vmaf <  target).max_by(|x, y| x.vmaf.total_cmp(&y.vmaf));
    if let (Some(a), Some(b)) = (above, below) {
        return ((a.crf, a.vmaf), (b.crf, b.vmaf));
    }
    let mut sorted: Vec<&Probe> = pts.iter().collect();
    sorted.sort_by(|x, y| (x.vmaf - target).abs().total_cmp(&(y.vmaf - target).abs()));
    ((sorted[0].crf, sorted[0].vmaf), (sorted[1].crf, sorted[1].vmaf))
}

fn round_to_step(v: f64, lo: f64, hi: f64) -> f64 {
    ((v / CRF_STEP).round() * CRF_STEP).clamp(lo, hi)
}

fn already(pts: &[Probe], crf: f64) -> bool {
    pts.iter().any(|p| (p.crf - crf).abs() < 1e-9)
}

/// Final CRF over the gathered probes. Quality floor sets an upper CRF bound, the
/// size cap a lower one; we take the highest CRF in the overlap. If the cap forces
/// a higher CRF than the floor allows, the cap wins. If nothing holds the floor,
/// the best-quality probe is used.
fn decide(pts: &[Probe], target: f64, cap: f64, lo: f64) -> SolveResult {
    let floor = pts.iter().filter(|p| p.vmaf >= target)
        .max_by(|a, b| a.crf.total_cmp(&b.crf)).copied();
    let under_cap = pts.iter().filter(|p| p.size_pct <= cap)
        .min_by(|a, b| a.crf.total_cmp(&b.crf)).copied();

    let res = |p: Probe, outcome| SolveResult { crf: p.crf, vmaf: p.vmaf, size_pct: p.size_pct, outcome };

    match (floor, under_cap) {
        (Some(fl), Some(sz)) if sz.crf > fl.crf + 1e-9 => res(sz, SolveOutcome::CapBinding),
        (Some(fl), _) => res(fl, SolveOutcome::Met),
        (None, _) => pts.iter()
            .max_by(|a, b| a.vmaf.total_cmp(&b.vmaf))
            .map(|&p| res(p, SolveOutcome::FloorUnreachable))
            .unwrap_or(SolveResult { crf: lo, vmaf: f64::NAN, size_pct: f64::NAN, outcome: SolveOutcome::FloorUnreachable }),
    }
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

    fn p(crf: f64, vmaf: f64, size_pct: f64) -> Probe {
        Probe { crf, vmaf, size_pct }
    }

    #[test]
    fn model_picked_by_height() {
        assert_eq!(model_for_height(720), MODEL_1080);
        assert_eq!(model_for_height(1080), MODEL_1080);
        assert_eq!(model_for_height(1439), MODEL_1080);
        assert_eq!(model_for_height(1440), MODEL_2160);
        assert_eq!(model_for_height(2160), MODEL_2160);
    }

    #[test]
    fn round_to_step_quarters_and_clamps() {
        assert_eq!(round_to_step(28.1, 14.0, 45.0), 28.0);
        assert_eq!(round_to_step(28.2, 14.0, 45.0), 28.25);
        assert_eq!(round_to_step(28.4, 14.0, 45.0), 28.5);
        assert_eq!(round_to_step(10.0, 14.0, 45.0), 14.0);
        assert_eq!(round_to_step(99.0, 14.0, 45.0), 45.0);
    }

    #[test]
    fn chunk_size_pct_uses_actual_source_bytes() {
        // cumulative bytes by frame: frames 0..=2 are 100+200+300 = 600 source bytes
        let cum = [0u64, 100, 300, 600, 1000];
        assert_eq!(chunk_size_pct(300, &cum, 0, 2), 50.0);
        assert_eq!(chunk_size_pct(200, &cum, 3, 3), 50.0);
        // out of range or empty index disables the cap (returns 0)
        assert_eq!(chunk_size_pct(300, &cum, 0, 99), 0.0);
        assert_eq!(chunk_size_pct(300, &[], 0, 2), 0.0);
    }

    #[test]
    fn interpolate_hits_crossing() {
        // vmaf 97 @ crf30, 93 @ crf40, target 95 -> 35
        let pts = vec![p(30.0, 97.0, 0.0), p(40.0, 93.0, 0.0)];
        assert!((interpolate_crf(&pts, 95.0) - 35.0).abs() < 1e-6);
    }

    #[test]
    fn decide_picks_highest_crf_above_floor() {
        // target 95, all under cap: highest CRF with vmaf >= 95 is 36
        let pts = vec![p(30.0, 96.0, 50.0), p(32.0, 95.2, 45.0), p(36.0, 95.0, 40.0), p(40.0, 92.0, 35.0)];
        let r = decide(&pts, 95.0, 90.0, 14.0);
        assert_eq!(r.crf, 36.0);
        assert!(matches!(r.outcome, SolveOutcome::Met));
    }

    #[test]
    fn decide_cap_binds_over_floor() {
        // floor CRF (20) is over the size cap; a higher CRF (25) is under it -> cap wins
        let pts = vec![p(20.0, 96.0, 120.0), p(25.0, 93.0, 80.0)];
        let r = decide(&pts, 95.0, 90.0, 14.0);
        assert_eq!(r.crf, 25.0);
        assert!(matches!(r.outcome, SolveOutcome::CapBinding));
    }

    #[test]
    fn decide_floor_unreachable_uses_best_quality() {
        // all below floor -> highest vmaf (crf 30)
        let pts = vec![p(30.0, 90.0, 50.0), p(35.0, 88.0, 40.0), p(40.0, 85.0, 30.0)];
        let r = decide(&pts, 95.0, 90.0, 14.0);
        assert_eq!(r.crf, 30.0);
        assert!(matches!(r.outcome, SolveOutcome::FloorUnreachable));
    }

    #[test]
    fn seed_uses_encoder_crf_when_present() {
        let mut config = Config {
            encoder: Some(crate::config::Encoder::SvtAv1),
            ..Default::default()
        };
        config.encoder_params.insert("crf".into(), toml::Value::Integer(28));
        assert_eq!(seed_crf(&config, 14.0, 45.0), 28.0);
        // out of range clamps
        config.encoder_params.insert("crf".into(), toml::Value::Integer(60));
        assert_eq!(seed_crf(&config, 14.0, 45.0), 45.0);
        // absent gives midpoint
        config.encoder_params.clear();
        assert_eq!(seed_crf(&config, 18.0, 44.0), 31.0);
    }
}
