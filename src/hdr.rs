use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

use crate::paths::external_bin;

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
    let out = tokio::process::Command::new(external_bin("ffprobe"))
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
            tracing::warn!("ffprobe HDR JSON parse failed: {e:#}");
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
        if info.content_light_level.is_none()
            && let (Some(mc), Some(ma)) = (&sd.max_content, &sd.max_average)
        {
            info.content_light_level =
                Some(format!("{},{}", val_to_i64(mc), val_to_i64(ma)));
        }
        if info.mastering_display.is_none()
            && let (Some(rx), Some(ry), Some(gx), Some(gy), Some(bx), Some(by),
                    Some(wpx), Some(wpy), Some(lmn), Some(lmx)) = (
                &sd.red_x, &sd.red_y, &sd.green_x, &sd.green_y,
                &sd.blue_x, &sd.blue_y, &sd.white_point_x, &sd.white_point_y,
                &sd.min_luminance, &sd.max_luminance,
            )
        {
            let (gx, gy)   = (val_to_f64(gx),  val_to_f64(gy));
            let (bx, by)   = (val_to_f64(bx),  val_to_f64(by));
            let (rx, ry)   = (val_to_f64(rx),  val_to_f64(ry));
            let (wpx, wpy) = (val_to_f64(wpx), val_to_f64(wpy));
            let (lmx, lmn) = (val_to_f64(lmx), val_to_f64(lmn));
            info.mastering_display = Some(format!(
                "G({gx:.4},{gy:.4})B({bx:.4},{by:.4})R({rx:.4},{ry:.4})\
                 WP({wpx:.4},{wpy:.4})L({lmx:.4},{lmn:.4})"
            ));
        }
    }

    // HLG has no static metadata by design; warn only for HDR10/HDR10+/DV.
    if info.is_hdr() && info.hdr_type != "HLG"
        && (info.content_light_level.is_none() || info.mastering_display.is_none())
    {
        let mut missing = Vec::new();
        if info.content_light_level.is_none() { missing.push("MaxCLL/MaxFALL"); }
        if info.mastering_display.is_none()   { missing.push("Mastering Display"); }
        tracing::warn!("HDR metadata incomplete - missing: {}", missing.join(", "));
    }

    Ok(info)
}

// ffprobe name → ITU-T H.273 numeric code (same values used by SVT-AV1)
fn map_color_primaries(s: &str) -> Option<u32> {
    Some(match s {
        "bt709"     => 1,
        "bt470m"    => 4,
        "bt470bg"   => 5,
        "smpte170m" => 6,
        "smpte240m" => 7,
        "film"      => 8,
        "bt2020"    => 9,
        "smpte428"  => 10,
        "smpte431"  => 11,
        "smpte432"  => 12,
        "ebu3213"   => 22,
        _           => return None,
    })
}

fn map_transfer(s: &str) -> Option<u32> {
    Some(match s {
        "bt709"        => 1,
        "gamma22"      => 4,
        "gamma28"      => 5,
        "smpte170m"    => 6,
        "smpte240m"    => 7,
        "linear"       => 8,
        "log"          => 9,
        "log_sqrt"     => 10,
        "iec61966-2-4" => 11,
        "bt1361"       => 12,
        "iec61966-2-1" => 13,
        "bt2020-10"    => 14,
        "bt2020-12"    => 15,
        "smpte2084"    => 16,
        "smpte428"     => 17,
        "arib-std-b67" => 18,
        _              => return None,
    })
}

fn map_matrix(s: &str) -> Option<u32> {
    Some(match s {
        "gbr"                => 0,
        "bt709"              => 1,
        "fcc"                => 4,
        "bt470bg"            => 5,
        "smpte170m"          => 6,
        "smpte240m"          => 7,
        "ycgco"              => 8,
        "bt2020nc"           => 9,
        "bt2020c"            => 10,
        "smpte2085"          => 11,
        "chroma-derived-nc"  => 12,
        "chroma-derived-c"   => 13,
        "ictcp"              => 14,
        _                    => return None,
    })
}

// SVT-AV1 --chroma-sample-position: 0=unknown, 1=vertical (left), 2=colocated (topleft).
// Other ffprobe locations have no AV1 equivalent.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h273_color_primaries() {
        assert_eq!(map_color_primaries("bt709"),    Some(1));
        assert_eq!(map_color_primaries("bt2020"),   Some(9));
        assert_eq!(map_color_primaries("smpte432"), Some(12));
        assert_eq!(map_color_primaries("ebu3213"),  Some(22));
        assert_eq!(map_color_primaries("unknown"),  None);
        assert_eq!(map_color_primaries(""),         None);
    }

    #[test]
    fn h273_transfer() {
        assert_eq!(map_transfer("smpte2084"),    Some(16));
        assert_eq!(map_transfer("arib-std-b67"), Some(18));
        assert_eq!(map_transfer("bt2020-10"),    Some(14));
        assert_eq!(map_transfer("nope"),         None);
    }

    #[test]
    fn h273_matrix_uses_ffprobe_shortnames() {
        assert_eq!(map_matrix("bt2020nc"), Some(9));
        assert_eq!(map_matrix("bt2020c"),  Some(10));
        assert_eq!(map_matrix("ictcp"),    Some(14));
    }

    #[test]
    fn hdr_type_detection() {
        let mut i = HdrInfo::default();
        assert!(!i.is_hdr());
        i.hdr_type = "SDR".into();
        assert!(!i.is_hdr());
        i.hdr_type = "HDR10".into();
        assert!(i.is_hdr());
    }

    #[test]
    fn encoder_args_only_for_set_fields() {
        let i = HdrInfo {
            color_primaries: Some(9),
            transfer_characteristics: Some(16),
            ..Default::default()
        };
        let args = i.encoder_args();
        assert_eq!(args, vec![
            "--color-primaries", "9",
            "--transfer-characteristics", "16",
        ]);
    }

    #[test]
    fn val_to_f64_parses_rational() {
        let v = serde_json::Value::String("50000/10000".into());
        assert_eq!(val_to_f64(&v), 5.0);
        let v = serde_json::Value::String("not-a-number".into());
        assert_eq!(val_to_f64(&v), 0.0);
    }
}
