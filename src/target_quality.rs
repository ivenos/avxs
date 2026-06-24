use anyhow::{Context, Result};
use std::path::Path;

use crate::config::{Config, TargetQualityConfig};
use crate::encode::{self, EncodeOptions};
use crate::resume::SceneEntry;
use crate::vmaf::{self, MeasureOpts};

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
        tracing::debug!("chunk {} probe crf {crf} -> vmaf {v:.2}", scene.padded_index());
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

    let result = vmaf::measure(&MeasureOpts {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_brackets_target() {
        // vmaf 97 @ crf30, vmaf 93 @ crf40, target 95 -> crf35
        let pts = vec![(30u32, 97.0), (40u32, 93.0)];
        assert_eq!(interpolate(&pts, 95.0, 10, 60), 35);
    }

    #[test]
    fn interpolate_clamps_to_bounds() {
        let pts = vec![(30u32, 99.5), (32u32, 99.0)];
        // target far below measured range -> push toward hi, clamped
        assert_eq!(interpolate(&pts, 80.0, 18, 45), 45);
    }

    #[test]
    fn pick_best_prefers_highest_crf_above_floor() {
        // target 95, tol_under 0.5 -> floor 94.5; crf32@95.2 and crf36@94.6 both ok -> 36
        let pts = vec![(30u32, 96.0), (32u32, 95.2), (36u32, 94.6), (40u32, 92.0)];
        assert_eq!(pick_best(&pts, 95.0, 0.5, 18), 36);
    }

    #[test]
    fn pick_best_falls_back_to_best_quality() {
        // all below floor -> highest vmaf wins (crf 30)
        let pts = vec![(30u32, 90.0), (35u32, 88.0), (40u32, 85.0)];
        assert_eq!(pick_best(&pts, 95.0, 0.5, 18), 30);
    }

    #[test]
    fn seed_uses_encoder_crf_when_present() {
        let mut config = Config {
            encoder: Some(crate::config::Encoder::SvtAv1),
            encoder_params: std::collections::HashMap::new(),
            avxs: Default::default(),
            audio: Default::default(),
            subtitles: Default::default(),
            scene_detection: Default::default(),
            target_quality: None,
        };
        config.encoder_params.insert("crf".into(), toml::Value::Integer(28));
        assert_eq!(seed_crf(&config, 18, 45), 28);
        // out of range clamps
        config.encoder_params.insert("crf".into(), toml::Value::Integer(60));
        assert_eq!(seed_crf(&config, 18, 45), 45);
        // absent -> midpoint
        config.encoder_params.clear();
        assert_eq!(seed_crf(&config, 18, 44), 31);
    }
}
