use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct HdrInfo {
    pub hdr_type: String,
    pub color_primaries: Option<u32>,
    pub transfer_characteristics: Option<u32>,
    pub matrix_coefficients: Option<u32>,
    pub chroma_sample_position: Option<u32>,
    pub content_light_level: Option<String>,
    pub mastering_display: Option<String>,
}

impl HdrInfo {
    pub fn is_hdr(&self) -> bool {
        !self.hdr_type.is_empty() && self.hdr_type != "SDR"
    }

    pub fn encoder_args(&self) -> Vec<String> {
        let mut args: Vec<String> = Vec::new();
        if let Some(cp) = self.color_primaries {
            args.extend_from_slice(&["--color-primaries".into(), cp.to_string()]);
        }
        if let Some(tc) = self.transfer_characteristics {
            args.extend_from_slice(&["--transfer-characteristics".into(), tc.to_string()]);
        }
        if let Some(mc) = self.matrix_coefficients {
            args.extend_from_slice(&["--matrix-coefficients".into(), mc.to_string()]);
        }
        if let Some(csp) = self.chroma_sample_position {
            args.extend_from_slice(&["--chroma-sample-position".into(), csp.to_string()]);
        }
        if let Some(ref cll) = self.content_light_level {
            args.extend_from_slice(&["--content-light".into(), cll.clone()]);
        }
        if let Some(ref mdl) = self.mastering_display {
            args.extend_from_slice(&["--mastering-display".into(), mdl.clone()]);
        }
        args
    }
}

#[derive(Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    frames: Vec<ProbeFrame>,
}

#[derive(Deserialize, Default)]
struct ProbeStream {
    #[serde(default)]
    color_primaries: String,
    #[serde(default)]
    color_transfer: String,
    #[serde(default)]
    color_space: String,
    #[serde(default)]
    chroma_location: String,
}

#[derive(Deserialize)]
struct ProbeFrame {
    #[serde(default)]
    side_data_list: Vec<SideData>,
}

#[derive(Deserialize)]
struct SideData {
    #[serde(default)]
    side_data_type: String,
    // Content Light Level
    max_content: Option<serde_json::Value>,
    max_average: Option<serde_json::Value>,
    // Mastering Display
    red_x: Option<serde_json::Value>,
    red_y: Option<serde_json::Value>,
    green_x: Option<serde_json::Value>,
    green_y: Option<serde_json::Value>,
    blue_x: Option<serde_json::Value>,
    blue_y: Option<serde_json::Value>,
    white_point_x: Option<serde_json::Value>,
    white_point_y: Option<serde_json::Value>,
    min_luminance: Option<serde_json::Value>,
    max_luminance: Option<serde_json::Value>,
}

pub async fn detect(source_file: &Path) -> Result<HdrInfo> {
    let out = tokio::process::Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-read_intervals", "%+#1",
            "-show_entries", "stream=color_primaries,color_transfer,color_space,chroma_location",
            "-show_frames",
            "-show_entries", "frame=side_data_list",
            "-print_format", "json",
        ])
        .arg(source_file)
        .output()
        .await
        .context("ffprobe HDR detection")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!("ffprobe HDR detection failed: {stderr}");
        return Ok(HdrInfo::default());
    }

    let probe: ProbeOutput = match serde_json::from_slice(&out.stdout) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("ffprobe HDR JSON parse failed: {e}");
            return Ok(HdrInfo::default());
        }
    };

    let stream = probe.streams.into_iter().next().unwrap_or_default();

    let mut info = HdrInfo {
        color_primaries: map_color_primaries(&stream.color_primaries),
        transfer_characteristics: map_transfer(&stream.color_transfer),
        matrix_coefficients: map_matrix(&stream.color_space),
        chroma_sample_position: map_chroma(&stream.chroma_location),
        ..Default::default()
    };

    let side_data = probe.frames.into_iter().next()
        .map(|f| f.side_data_list)
        .unwrap_or_default();

    let has_side_type = |needle: &str| {
        side_data.iter().any(|s| s.side_data_type.to_lowercase().contains(needle))
    };

    info.hdr_type = if has_side_type("dovi") || has_side_type("dolby") {
        "Dolby Vision".into()
    } else if has_side_type("hdr10+") || has_side_type("hdr_dynamic") {
        "HDR10+".into()
    } else if stream.color_transfer == "smpte2084" {
        "HDR10".into()
    } else if stream.color_transfer == "arib-std-b67" {
        "HLG".into()
    } else {
        "SDR".into()
    };

    for sd in &side_data {
        if info.content_light_level.is_none() {
            if let (Some(mc), Some(ma)) = (&sd.max_content, &sd.max_average) {
                info.content_light_level =
                    Some(format!("{},{}", val_to_i64(mc), val_to_i64(ma)));
            }
        }
        if info.mastering_display.is_none() {
            let all = [
                &sd.red_x, &sd.red_y, &sd.green_x, &sd.green_y,
                &sd.blue_x, &sd.blue_y, &sd.white_point_x, &sd.white_point_y,
                &sd.min_luminance, &sd.max_luminance,
            ];
            if all.iter().all(|v| v.is_some()) {
                let gx  = val_to_f64(sd.green_x.as_ref().unwrap());
                let gy  = val_to_f64(sd.green_y.as_ref().unwrap());
                let bx  = val_to_f64(sd.blue_x.as_ref().unwrap());
                let by  = val_to_f64(sd.blue_y.as_ref().unwrap());
                let rx  = val_to_f64(sd.red_x.as_ref().unwrap());
                let ry  = val_to_f64(sd.red_y.as_ref().unwrap());
                let wpx = val_to_f64(sd.white_point_x.as_ref().unwrap());
                let wpy = val_to_f64(sd.white_point_y.as_ref().unwrap());
                let lmx = val_to_f64(sd.max_luminance.as_ref().unwrap());
                let lmn = val_to_f64(sd.min_luminance.as_ref().unwrap());
                info.mastering_display = Some(format!(
                    "G({gx:.4},{gy:.4})B({bx:.4},{by:.4})R({rx:.4},{ry:.4})\
                     WP({wpx:.4},{wpy:.4})L({lmx:.4},{lmn:.4})"
                ));
            }
        }
    }

    if info.is_hdr() && (info.content_light_level.is_none() || info.mastering_display.is_none()) {
        let mut missing = Vec::new();
        if info.content_light_level.is_none() { missing.push("MaxCLL/MaxFALL"); }
        if info.mastering_display.is_none()   { missing.push("Mastering Display"); }
        tracing::warn!("HDR metadata incomplete — missing: {}", missing.join(", "));
    }

    Ok(info)
}

// ffprobe name → ITU-T H.273 numeric code (same values used by SVT-AV1)
fn map_color_primaries(s: &str) -> Option<u32> {
    Some(match s {
        "bt709"     => 1,
        "smpte170m" => 6,
        "bt2020"    => 9,
        "smpte432"  => 12,
        _           => return None,
    })
}

fn map_transfer(s: &str) -> Option<u32> {
    Some(match s {
        "bt709"        => 1,
        "smpte170m"    => 6,
        "smpte2084"    => 16,
        "arib-std-b67" => 18,
        _              => return None,
    })
}

fn map_matrix(s: &str) -> Option<u32> {
    Some(match s {
        "bt709"     => 1,
        "smpte170m" => 6,
        "bt2020nc"  => 9,
        "bt2020c"   => 10,
        _           => return None,
    })
}

fn map_chroma(s: &str) -> Option<u32> {
    Some(match s {
        "left"    => 1,
        "topleft" => 2,
        _         => return None,
    })
}

fn val_to_f64(v: &serde_json::Value) -> f64 {
    match v {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => {
            if let Some((num, den)) = s.split_once('/') {
                let n: f64 = num.trim().parse().unwrap_or(0.0);
                let d: f64 = den.trim().parse().unwrap_or(1.0);
                if d != 0.0 { n / d } else { 0.0 }
            } else {
                s.parse().unwrap_or(0.0)
            }
        }
        _ => 0.0,
    }
}

fn val_to_i64(v: &serde_json::Value) -> i64 {
    match v {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        serde_json::Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}
