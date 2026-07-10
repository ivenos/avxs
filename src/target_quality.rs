use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::{Config, TargetQualityConfig};
use crate::encode::{self, EncodeOptions};
use crate::ext::external_bin;
use crate::ffms2::Crop;
use crate::resume::SceneEntry;

// CRF granularity of the SVT-AV1 encoders (and the HDR fork): quarter steps.
const CRF_STEP: f64 = 0.25;

// Nominal JOD drop per CRF step, used only to seed the first interpolation from a
// single probe. Calibrated on 4K HDR SVT-AV1 material (~0.02-0.03 JOD per CRF near
// the target zone); the search refines it from the real probes.
const NOMINAL_JOD_PER_CRF: f64 = 0.025;

/// CVVDP display model for the output. HDR selects the PQ or HLG display by the
/// encoded transfer; SDR picks the 4K or FHD display by output height.
pub fn display_model_for(output_height: u32, hdr: bool, hlg: bool) -> &'static str {
    if hdr && hlg {
        "standard_hdr_hlg"
    } else if hdr {
        "standard_hdr_pq"
    } else if output_height >= 1440 {
        "standard_4k"
    } else {
        "standard_fhd"
    }
}

/// True when the auto-HDR args select the HLG transfer (arib-std-b67 = 18);
/// otherwise HDR is treated as PQ (HDR10, plus the DV/HDR10+ HDR10 fallback).
pub fn hdr_args_are_hlg(hdr_args: &[String]) -> bool {
    hdr_args.windows(2).any(|w| w[0] == "--transfer-characteristics" && w[1] == "18")
}

/// A Vulkan device reported by FFVship.
#[derive(Clone, Debug)]
pub struct GpuSelection {
    pub id: u32,
    pub label: String,
    /// False for a software rasterizer (llvmpipe); target_quality rejects those.
    pub hardware: bool,
}

impl GpuSelection {
    pub fn describe(&self) -> String {
        format!("gpu {} {}", self.id, self.label)
    }
}

/// Confirms FFVship runs and a hardware GPU is available. target_quality needs a GPU;
/// a software-only Vulkan device (llvmpipe) or none is rejected with a clear error,
/// because CVVDP on the CPU is far too slow to be practical.
pub async fn ensure_available() -> Result<GpuSelection> {
    let out = tokio::process::Command::new(external_bin("FFVship"))
        .arg("--list-gpu")
        .output()
        .await
        .context("run FFVship --list-gpu (is the FFVship tool bundled?)")?;
    // With no usable Vulkan driver at all, FFVship aborts creating the instance.
    if !out.status.success() {
        bail!(
            "target_quality requires a GPU, but FFVship could not initialize Vulkan:\n{}\n\
             Provide a GPU (Intel/AMD: pass the render device /dev/dri; NVIDIA: nvidia-container-toolkit).",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    match select_gpu(&text) {
        Some(g) if g.hardware => Ok(g),
        Some(g) => bail!(
            "target_quality requires a GPU, but FFVship found only a software Vulkan device ({}). \
             Provide a GPU (Intel/AMD: pass the render device /dev/dri; NVIDIA: nvidia-container-toolkit), \
             or remove [target_quality].",
            g.label
        ),
        None => bail!(
            "target_quality requires a GPU, but FFVship found no Vulkan device. \
             Provide a GPU (Intel/AMD: pass the render device /dev/dri; NVIDIA: nvidia-container-toolkit)."
        ),
    }
}

/// Parses `FFVship --list-gpu` ("GPU <id>: <name>") and returns the first hardware
/// device, else the first software device (llvmpipe) so callers can tell them apart.
fn select_gpu(list: &str) -> Option<GpuSelection> {
    let mut devices: Vec<GpuSelection> = Vec::new();
    for line in list.lines() {
        let Some(rest) = line.trim().strip_prefix("GPU ") else { continue };
        let Some((id_str, name)) = rest.split_once(':') else { continue };
        let Ok(id) = id_str.trim().parse::<u32>() else { continue };
        let label = name.trim().to_string();
        let low = label.to_lowercase();
        let hardware =
            !low.contains("llvmpipe") && !low.contains("software") && !low.contains("swrast");
        devices.push(GpuSelection { id, label, hardware });
    }
    devices
        .iter()
        .find(|d| d.hardware)
        .cloned()
        .or_else(|| devices.into_iter().next())
}

pub struct ProbeContext<'a> {
    pub source: &'a Path,
    pub index: &'a Path,
    pub temp_dir: &'a Path,
    pub config: &'a Config,
    pub opts: &'a EncodeOptions,
    pub tq: &'a TargetQualityConfig,
    pub display_model: &'a str,
    pub gpu_id: u32,
    /// Source dimensions, needed to turn avxs crop (offset+size) into FFVship edge crops.
    pub source_width: u32,
    pub source_height: u32,
    pub n_threads: usize,
    pub stem: &'a str,
    /// Cumulative source byte sizes by frame (len = frames + 1); empty disables the cap.
    pub source_byte_index: &'a [u64],
}

#[derive(Clone, Copy)]
struct Probe {
    crf: f64,
    jod: f64,
    size_pct: f64,
}

/// Why the search settled on its CRF, for logging.
pub enum SolveOutcome {
    /// Highest CRF that holds the JOD floor within the size cap.
    Met,
    /// Size cap forced a higher CRF than the floor allowed; JOD is below target.
    CapBinding,
    /// Floor unreachable in the CRF range; best-quality probe used.
    FloorUnreachable,
}

pub struct SolveResult {
    pub crf: f64,
    pub jod: f64,
    pub size_pct: f64,
    pub outcome: SolveOutcome,
}

/// Finds the highest CRF whose JOD holds the floor `tq.jod` and whose size stays
/// under `tq.max_encoded_percent`. JOD falls monotonically as CRF rises, so this is
/// a threshold search: interpolated binary search on a 0.25 grid.
pub fn solve_chunk_crf(ctx: &ProbeContext, scene: &SceneEntry) -> Result<SolveResult> {
    let lo = ctx.tq.min_crf as f64;
    let hi = ctx.tq.max_crf as f64;
    let target = ctx.tq.jod;
    let tol = ctx.tq.tolerance;
    let cap = ctx.tq.max_encoded_percent;
    let key = scene.padded_index();

    let mut pts: Vec<Probe> = Vec::new();
    let mut crf = round_to_step(seed_crf(ctx.config, lo, hi), lo, hi);

    for i in 0..ctx.tq.max_probes {
        let (jod, size_pct) = probe_once(ctx, scene, crf)?;
        tracing::info!(
            "[{}] chunk {key} probe {}/{} crf {crf} gives JOD {jod:.3}, {size_pct:.0}% size",
            ctx.stem, i + 1, ctx.tq.max_probes
        );
        pts.push(Probe { crf, jod, size_pct });

        // early stop: just above the floor, within the size cap, after min_probes
        if i + 1 >= ctx.tq.min_probes
            && jod >= target && jod <= target + tol && size_pct <= cap
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

/// Encodes the chunk at `crf` (probe preset), measures CVVDP JOD, and reports the
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
        crop: ctx.opts.crop,
        source_width: ctx.source_width,
        source_height: ctx.source_height,
        display_model: ctx.display_model,
        gpu_id: ctx.gpu_id,
        n_threads: ctx.n_threads,
        tag: &tag,
    });
    let _ = std::fs::remove_file(&probe);
    let jod = result?;
    let size_pct = chunk_size_pct(size_bytes, ctx.source_byte_index, scene.start_frame, scene.end_frame);
    Ok((jod, size_pct))
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
    let pass: Vec<f64> = pts.iter().filter(|p| p.jod >= target).map(|p| p.crf).collect();
    let fail: Vec<f64> = pts.iter().filter(|p| p.jod <  target).map(|p| p.crf).collect();

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

/// Linear (secant) estimate of the CRF that yields `target` JOD.
fn interpolate_crf(pts: &[Probe], target: f64) -> f64 {
    if pts.len() == 1 {
        return pts[0].crf + (pts[0].jod - target) / NOMINAL_JOD_PER_CRF;
    }
    let (a, b) = bracket_pts(pts, target);
    if (a.1 - b.1).abs() < 1e-6 {
        return (a.0 + b.0) / 2.0;
    }
    let slope = (b.0 - a.0) / (b.1 - a.1);
    a.0 + slope * (target - a.1)
}

/// Two (crf, jod) points to interpolate between: one at/above and one below the
/// target if possible, otherwise the two closest in JOD.
fn bracket_pts(pts: &[Probe], target: f64) -> ((f64, f64), (f64, f64)) {
    let above = pts.iter().filter(|p| p.jod >= target).min_by(|x, y| x.jod.total_cmp(&y.jod));
    let below = pts.iter().filter(|p| p.jod <  target).max_by(|x, y| x.jod.total_cmp(&y.jod));
    if let (Some(a), Some(b)) = (above, below) {
        return ((a.crf, a.jod), (b.crf, b.jod));
    }
    let mut sorted: Vec<&Probe> = pts.iter().collect();
    sorted.sort_by(|x, y| (x.jod - target).abs().total_cmp(&(y.jod - target).abs()));
    ((sorted[0].crf, sorted[0].jod), (sorted[1].crf, sorted[1].jod))
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
    let floor = pts.iter().filter(|p| p.jod >= target)
        .max_by(|a, b| a.crf.total_cmp(&b.crf)).copied();
    let under_cap = pts.iter().filter(|p| p.size_pct <= cap)
        .min_by(|a, b| a.crf.total_cmp(&b.crf)).copied();

    let res = |p: Probe, outcome| SolveResult { crf: p.crf, jod: p.jod, size_pct: p.size_pct, outcome };

    match (floor, under_cap) {
        (Some(fl), Some(sz)) if sz.crf > fl.crf + 1e-9 => res(sz, SolveOutcome::CapBinding),
        (Some(fl), _) => res(fl, SolveOutcome::Met),
        (None, _) => pts.iter()
            .max_by(|a, b| a.jod.total_cmp(&b.jod))
            .map(|&p| res(p, SolveOutcome::FloorUnreachable))
            .unwrap_or(SolveResult { crf: lo, jod: f64::NAN, size_pct: f64::NAN, outcome: SolveOutcome::FloorUnreachable }),
    }
}

struct MeasureOpts<'a> {
    distorted: &'a Path,
    source: &'a Path,
    /// avxs's existing FFMS2 index for the source, reused read-only by FFVship.
    index: &'a Path,
    work_dir: &'a Path,
    /// First source frame of the chunk; the probe holds those frames from 0.
    start: u64,
    crop: Option<Crop>,
    source_width: u32,
    source_height: u32,
    display_model: &'a str,
    gpu_id: u32,
    n_threads: usize,
    /// Unique suffix for the per-measurement json file.
    tag: &'a str,
}

/// Removes its paths on drop so the json never lingers after a measurement.
struct Cleanup(Vec<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Whole-chunk CVVDP JOD of `distorted` (the probe, frames from 0) against the
/// source starting at `start`. FFVship decodes both via ffms2, crops the source to
/// match the encode, resizes the encode on any size mismatch, and writes per-frame
/// cumulative JOD; the last value is the chunk score.
fn measure(m: &MeasureOpts) -> Result<f64> {
    let json = m.work_dir.join(format!("cvvdp_{}.json", m.tag));
    let _cleanup = Cleanup(vec![json.clone()]);

    let mut cmd = std::process::Command::new(external_bin("FFVship"));
    cmd.arg("-s").arg(m.source)
        .arg("-e").arg(m.distorted)
        .args(["-m", "CVVDP"])
        .arg("--source-index").arg(m.index)
        .args(["--start", &m.start.to_string()])
        .args(["--encoded-offset", &format!("-{}", m.start)])
        .args(["--displayModel", m.display_model])
        .args(["--gpu-id", &m.gpu_id.to_string()])
        .args(["-t", &m.n_threads.to_string()])
        .args(["-g", "3"])
        .arg("--json").arg(&json);

    // avxs crop is offset+size in source space; FFVship wants per-edge amounts.
    if let Some(c) = m.crop {
        let right = m.source_width.saturating_sub(c.x + c.w);
        let bottom = m.source_height.saturating_sub(c.y + c.h);
        cmd.args(["--cropLeftSource", &c.x.to_string()])
            .args(["--cropTopSource", &c.y.to_string()])
            .args(["--cropRightSource", &right.to_string()])
            .args(["--cropBottomSource", &bottom.to_string()]);
    }

    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("run FFVship")?;
    if !out.status.success() {
        bail!("FFVship failed:\n{}", String::from_utf8_lossy(&out.stderr));
    }

    let raw = std::fs::read_to_string(&json)
        .with_context(|| format!("read FFVship json: {}", json.display()))?;
    parse_cvvdp(&raw)
}

/// CVVDP JSON is `[[cum_0_0], [cum_0_1], ...]`; the last row's value is the score
/// of the whole clip (frame 0 to last).
fn parse_cvvdp(raw: &str) -> Result<f64> {
    let rows: Vec<Vec<f64>> = serde_json::from_str(raw).context("parse FFVship CVVDP json")?;
    rows.last()
        .and_then(|r| r.last())
        .copied()
        .ok_or_else(|| anyhow!("FFVship CVVDP json had no scores"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(crf: f64, jod: f64, size_pct: f64) -> Probe {
        Probe { crf, jod, size_pct }
    }

    #[test]
    fn display_model_picked_by_height_and_hdr() {
        assert_eq!(display_model_for(720, false, false), "standard_fhd");
        assert_eq!(display_model_for(1080, false, false), "standard_fhd");
        assert_eq!(display_model_for(1439, false, false), "standard_fhd");
        assert_eq!(display_model_for(1440, false, false), "standard_4k");
        assert_eq!(display_model_for(2160, false, false), "standard_4k");
        assert_eq!(display_model_for(2160, true, false), "standard_hdr_pq");
        // HLG picks the HLG display even at 4K
        assert_eq!(display_model_for(2160, true, true), "standard_hdr_hlg");
    }

    #[test]
    fn hlg_detected_from_transfer_arg() {
        let pq = vec!["--transfer-characteristics".to_string(), "16".to_string()];
        let hlg = vec!["--transfer-characteristics".to_string(), "18".to_string()];
        assert!(!hdr_args_are_hlg(&pq));
        assert!(hdr_args_are_hlg(&hlg));
        assert!(!hdr_args_are_hlg(&[]));
    }

    #[test]
    fn select_gpu_prefers_hardware() {
        let list = "GPU 0: NVIDIA GeForce RTX 5060 Ti\nGPU 1: llvmpipe (LLVM 22.1.7, 256 bits)\n";
        let g = select_gpu(list).unwrap();
        assert_eq!(g.id, 0);
        assert!(g.hardware);
    }

    #[test]
    fn select_gpu_picks_hardware_even_after_software() {
        let list = "GPU 0: llvmpipe (LLVM 22.1.7)\nGPU 1: Intel Graphics\n";
        let g = select_gpu(list).unwrap();
        assert_eq!(g.id, 1);
        assert!(g.hardware);
    }

    #[test]
    fn select_gpu_reports_software_only() {
        // software-only is detected (hardware=false); ensure_available then rejects it
        let g = select_gpu("GPU 0: llvmpipe (LLVM 22.1.7)\n").unwrap();
        assert_eq!(g.id, 0);
        assert!(!g.hardware);
    }

    #[test]
    fn parse_cvvdp_takes_last_cumulative() {
        assert!((parse_cvvdp("[[9.95],[9.90],[9.83]]").unwrap() - 9.83).abs() < 1e-9);
        assert!(parse_cvvdp("[]").is_err());
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
        let cum = [0u64, 100, 300, 600, 1000];
        assert_eq!(chunk_size_pct(300, &cum, 0, 2), 50.0);
        assert_eq!(chunk_size_pct(200, &cum, 3, 3), 50.0);
        assert_eq!(chunk_size_pct(300, &cum, 0, 99), 0.0);
        assert_eq!(chunk_size_pct(300, &[], 0, 2), 0.0);
    }

    #[test]
    fn interpolate_hits_crossing() {
        // jod 9.7 @ crf30, 9.3 @ crf40, target 9.5 -> 35
        let pts = vec![p(30.0, 9.7, 0.0), p(40.0, 9.3, 0.0)];
        assert!((interpolate_crf(&pts, 9.5) - 35.0).abs() < 1e-6);
    }

    #[test]
    fn decide_picks_highest_crf_above_floor() {
        // target 9.5, all under cap: highest CRF with jod >= 9.5 is 36
        let pts = vec![p(30.0, 9.6, 50.0), p(32.0, 9.52, 45.0), p(36.0, 9.5, 40.0), p(40.0, 9.2, 35.0)];
        let r = decide(&pts, 9.5, 90.0, 14.0);
        assert_eq!(r.crf, 36.0);
        assert!(matches!(r.outcome, SolveOutcome::Met));
    }

    #[test]
    fn decide_cap_binds_over_floor() {
        // floor CRF (20) is over the size cap; a higher CRF (25) is under it -> cap wins
        let pts = vec![p(20.0, 9.6, 120.0), p(25.0, 9.3, 80.0)];
        let r = decide(&pts, 9.5, 90.0, 14.0);
        assert_eq!(r.crf, 25.0);
        assert!(matches!(r.outcome, SolveOutcome::CapBinding));
    }

    #[test]
    fn decide_floor_unreachable_uses_best_quality() {
        // all below floor -> highest jod (crf 30)
        let pts = vec![p(30.0, 9.0, 50.0), p(35.0, 8.8, 40.0), p(40.0, 8.5, 30.0)];
        let r = decide(&pts, 9.5, 90.0, 14.0);
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
        config.encoder_params.insert("crf".into(), toml::Value::Integer(60));
        assert_eq!(seed_crf(&config, 14.0, 45.0), 45.0);
        config.encoder_params.clear();
        assert_eq!(seed_crf(&config, 18.0, 44.0), 31.0);
    }
}
