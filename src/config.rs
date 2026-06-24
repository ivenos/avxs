use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub encoder: Option<Encoder>,
    #[serde(default)]
    pub encoder_params: HashMap<String, toml::Value>,
    #[serde(default)]
    pub avxs: AvxsConfig,
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub subtitles: SubtitleConfig,
    #[serde(default)]
    pub scene_detection: SceneDetectionConfig,
    /// Per-chunk VMAF target instead of a fixed CRF. None = fixed CRF.
    pub target_quality: Option<TargetQualityConfig>,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Encoder {
    SvtAv1,
    SvtAv1Hdr,
}

#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum VideoMode {
    #[default]
    Encode,
    Copy,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct AvxsConfig {
    #[serde(default)]
    pub video: VideoMode,
    #[serde(default)]
    pub hdr: bool,
    #[serde(default)]
    pub crop: bool,
    #[serde(default)]
    pub keyint: bool,
    pub scale: Option<u32>,
    pub bit_depth: Option<u8>,
    #[serde(default)]
    pub keep_temp: bool,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct SubtitleConfig {
    #[serde(default)]
    pub mode: SubtitleMode,
    #[serde(default)]
    pub language_whitelist: Vec<String>,
}

#[derive(Debug, Deserialize, Default, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum SubtitleMode {
    #[default]
    Copy,
    Strip,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct AudioConfig {
    #[serde(default)]
    pub mode: AudioMode,
    pub codec: Option<String>,
    pub bitrate: Option<Bitrate>,
    #[serde(default)]
    pub options: HashMap<String, toml::Value>,
    #[serde(default)]
    pub language_whitelist: Vec<String>,
    /// Override for lossless sources; unset fields inherit from [audio].
    pub lossless: Option<AudioProfile>,
    #[serde(default)]
    pub codec_rules: HashMap<String, AudioProfile>,
}

/// Override whose unset fields inherit from [audio].
#[derive(Debug, Deserialize, Default, Clone)]
pub struct AudioProfile {
    pub mode: Option<AudioMode>,
    pub codec: Option<String>,
    pub bitrate: Option<Bitrate>,
    #[serde(default)]
    pub options: HashMap<String, toml::Value>,
}

/// A single bitrate, or a per-layout table keyed by layout name (plus `default`).
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
pub enum Bitrate {
    Single(String),
    PerLayout(HashMap<String, String>),
}

impl Bitrate {
    pub fn resolve(&self, channels: Option<u32>) -> Option<&str> {
        match self {
            Bitrate::Single(s) => Some(s.as_str()),
            Bitrate::PerLayout(map) => channels
                .and_then(|c| map.get(layout_name(c)))
                .or_else(|| map.get("default"))
                .map(String::as_str),
        }
    }
}

pub struct ResolvedAudio<'a> {
    pub mode: AudioMode,
    pub codec: Option<&'a str>,
    pub bitrate: Option<&'a Bitrate>,
    pub options: &'a HashMap<String, toml::Value>,
}

#[derive(Debug, Deserialize, Default, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    #[default]
    Copy,
    Encode,
}

/// Layout name for a channel count (per-layout bitrate key).
pub fn layout_name(channels: u32) -> &'static str {
    match channels {
        0 | 1 => "mono",
        2 => "stereo",
        3 => "3.0",
        4 => "quad",
        5 => "5.0",
        6 => "5.1",
        7 => "6.1",
        _ => "7.1",
    }
}

/// True if the output codec is lossless (bitrate then irrelevant).
pub fn output_is_lossless(codec: &str) -> bool {
    matches!(codec, "flac" | "alac" | "wavpack" | "tta") || codec.starts_with("pcm_")
}

/// Stringify a TOML value for the ffmpeg/encoder CLI (booleans as 1/0).
pub(crate) fn toml_value_to_arg(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s)  => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f)   => f.to_string(),
        toml::Value::Boolean(b) => if *b { "1".into() } else { "0".into() },
        other                   => other.to_string(),
    }
}

impl AudioConfig {
    /// Resolve a track: codec_rules, then lossless override, then [audio].
    pub fn resolve(&self, codec_name: &str, is_lossless: bool) -> ResolvedAudio<'_> {
        if let Some(rule) = self.codec_rules.get(codec_name) {
            return self.overlay(rule);
        }
        if is_lossless && let Some(p) = &self.lossless {
            return self.overlay(p);
        }
        ResolvedAudio {
            mode: self.mode,
            codec: self.codec.as_deref(),
            bitrate: self.bitrate.as_ref(),
            options: &self.options,
        }
    }

    /// Apply an override over [audio], inheriting unset fields.
    fn overlay<'a>(&'a self, ov: &'a AudioProfile) -> ResolvedAudio<'a> {
        ResolvedAudio {
            mode: ov.mode.unwrap_or(self.mode),
            codec: ov.codec.as_deref().or(self.codec.as_deref()),
            bitrate: ov.bitrate.as_ref().or(self.bitrate.as_ref()),
            options: if ov.options.is_empty() { &self.options } else { &ov.options },
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SceneDetectionSpeedConfig {
    #[default]
    Standard,
    Fast,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SceneDetectionConfig {
    /// Minimum number of frames between scene cuts.
    #[serde(default = "SceneDetectionConfig::default_min_scene_len")]
    pub min_scene_len: usize,
    /// Maximum scene length in seconds before an extra split is inserted.
    /// Set to 0 to disable. Ignored when `extra_split` > 0.
    #[serde(default = "SceneDetectionConfig::default_extra_split_sec")]
    pub extra_split_sec: u32,
    /// Maximum scene length in frames. Overrides `extra_split_sec` when > 0. Set to 0 to disable.
    #[serde(default)]
    pub extra_split: u32,
    /// Scene detection algorithm speed.
    #[serde(default)]
    pub speed: SceneDetectionSpeedConfig,
    /// Downscale height for scene detection only (e.g. 720). None = no extra downscale.
    #[serde(default)]
    pub downscale_height: Option<u32>,
}

impl Default for SceneDetectionConfig {
    fn default() -> Self {
        Self {
            min_scene_len: Self::default_min_scene_len(),
            extra_split_sec: Self::default_extra_split_sec(),
            extra_split: 0,
            speed: SceneDetectionSpeedConfig::default(),
            downscale_height: None,
        }
    }
}

impl SceneDetectionConfig {
    fn default_min_scene_len() -> usize { 24 }
    fn default_extra_split_sec() -> u32 { 10 }

    /// Returns the effective max chunk size in frames, or None if extra splitting is disabled.
    pub fn effective_extra_split_frames(&self, fps: f64) -> Option<usize> {
        if self.extra_split > 0 {
            Some(self.extra_split as usize)
        } else if self.extra_split_sec > 0 {
            let frames = (self.extra_split_sec as f64 * fps).round() as usize;
            if frames > 0 { Some(frames) } else { None }
        } else {
            None
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TargetQualityConfig {
    /// VMAF score to target per chunk.
    pub vmaf: f64,
    #[serde(default = "TargetQualityConfig::default_min_crf")]
    pub min_crf: u32,
    #[serde(default = "TargetQualityConfig::default_max_crf")]
    pub max_crf: u32,
    #[serde(default = "TargetQualityConfig::default_probes")]
    pub probes: u32,
    #[serde(default = "TargetQualityConfig::default_probe_preset")]
    pub probe_preset: u32,
    /// Accept a probe that lands up to this far below the target.
    #[serde(default = "TargetQualityConfig::default_tolerance_under")]
    pub tolerance_under: f64,
    /// Accept a probe that lands up to this far above the target.
    #[serde(default = "TargetQualityConfig::default_tolerance_over")]
    pub tolerance_over: f64,
}

impl TargetQualityConfig {
    fn default_min_crf() -> u32 { 18 }
    fn default_max_crf() -> u32 { 45 }
    fn default_probes() -> u32 { 4 }
    fn default_probe_preset() -> u32 { 13 }
    fn default_tolerance_under() -> f64 { 0.5 }
    fn default_tolerance_over() -> f64 { 2.0 }
}

impl Config {
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read encode.toml: {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parse encode.toml: {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.avxs.video != VideoMode::Copy && self.encoder.is_none() {
            bail!("encoder is required unless avxs.video = \"copy\"");
        }
        if let Some(d) = self.avxs.bit_depth
            && d != 8 && d != 10
        {
            bail!("avxs.bit_depth must be 8 or 10 (got {d})");
        }
        if let Some(tq) = &self.target_quality {
            if self.avxs.video == VideoMode::Copy {
                bail!("target_quality requires avxs.video = \"encode\"");
            }
            if !(tq.vmaf > 0.0 && tq.vmaf <= 100.0) {
                bail!("target_quality.vmaf must be in (0, 100] (got {})", tq.vmaf);
            }
            if tq.min_crf >= tq.max_crf {
                bail!("target_quality.min_crf must be < max_crf ({} >= {})", tq.min_crf, tq.max_crf);
            }
            if tq.max_crf > 63 {
                bail!("target_quality.max_crf must be <= 63 (got {})", tq.max_crf);
            }
            if tq.probes < 2 {
                bail!("target_quality.probes must be >= 2 (got {})", tq.probes);
            }
            if tq.probe_preset > 13 {
                bail!("target_quality.probe_preset must be 0..=13 (got {})", tq.probe_preset);
            }
            if tq.tolerance_under < 0.0 || tq.tolerance_over < 0.0 {
                bail!("target_quality tolerances must be >= 0");
            }
        }
        validate_audio("audio", self.audio.mode, self.audio.codec.as_deref(), self.audio.bitrate.as_ref())?;
        if let Some(p) = &self.audio.lossless {
            let r = self.audio.overlay(p);
            validate_audio("audio.lossless", r.mode, r.codec, r.bitrate)?;
        }
        for (source_codec, rule) in &self.audio.codec_rules {
            let r = self.audio.overlay(rule);
            validate_audio(&format!("audio.codec_rules.{source_codec}"), r.mode, r.codec, r.bitrate)?;
        }
        Ok(())
    }

    pub fn encoder_args(&self) -> Vec<String> {
        let mut entries: Vec<_> = self.encoder_params.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let mut args = Vec::with_capacity(entries.len() * 2);
        for (k, v) in entries {
            args.push(format!("--{k}"));
            args.push(toml_value_to_arg(v));
        }
        args
    }
}

/// Encode needs a codec; lossy codecs also need a bitrate.
fn validate_audio(ctx: &str, mode: AudioMode, codec: Option<&str>, bitrate: Option<&Bitrate>) -> Result<()> {
    if mode != AudioMode::Encode {
        return Ok(());
    }
    let Some(codec) = codec else {
        bail!("{ctx}: codec required when mode = encode");
    };
    if !output_is_lossless(codec) && bitrate.is_none() {
        bail!("{ctx}: bitrate required when mode = encode ({codec} is lossy)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_bit_depth(d: Option<u8>) -> Config {
        Config {
            encoder: Some(Encoder::SvtAv1),
            encoder_params: HashMap::new(),
            avxs: AvxsConfig { bit_depth: d, ..Default::default() },
            audio: AudioConfig::default(),
            subtitles: SubtitleConfig::default(),
            scene_detection: SceneDetectionConfig::default(),
            target_quality: None,
        }
    }

    #[test]
    fn bit_depth_8_and_10_are_valid() {
        cfg_with_bit_depth(Some(8)).validate().unwrap();
        cfg_with_bit_depth(Some(10)).validate().unwrap();
        cfg_with_bit_depth(None).validate().unwrap();
    }

    #[test]
    fn bit_depth_other_values_rejected() {
        for d in [0u8, 9, 12, 16] {
            let err = cfg_with_bit_depth(Some(d)).validate().unwrap_err();
            assert!(
                err.to_string().contains("bit_depth"),
                "expected bit_depth error for {d}, got: {err}"
            );
        }
    }

    fn audio(toml_str: &str) -> AudioConfig {
        toml::from_str(toml_str).expect("parse audio config")
    }

    #[test]
    fn video_mode_defaults_to_encode() {
        let c: Config = toml::from_str(r#"encoder = "svt-av1""#).unwrap();
        assert_eq!(c.avxs.video, VideoMode::Encode);
        let c: Config = toml::from_str("encoder = \"svt-av1\"\n[avxs]\nvideo = \"copy\"").unwrap();
        assert_eq!(c.avxs.video, VideoMode::Copy);
    }

    #[test]
    fn bitrate_parses_single_and_per_layout() {
        let a = audio(r#"bitrate = "192k""#);
        assert!(matches!(a.bitrate, Some(Bitrate::Single(ref s)) if s == "192k"));

        let a = audio(r#"bitrate = { stereo = "192k", "5.1" = "320k" }"#);
        assert!(matches!(a.bitrate, Some(Bitrate::PerLayout(_))));
    }

    #[test]
    fn layout_name_maps_channel_counts() {
        assert_eq!(layout_name(1), "mono");
        assert_eq!(layout_name(2), "stereo");
        assert_eq!(layout_name(6), "5.1");
        assert_eq!(layout_name(8), "7.1");
        assert_eq!(layout_name(16), "7.1");
    }

    #[test]
    fn bitrate_resolve_by_channels_with_default() {
        let b = Bitrate::PerLayout(HashMap::from([
            ("stereo".into(), "192k".into()),
            ("5.1".into(), "320k".into()),
            ("default".into(), "256k".into()),
        ]));
        assert_eq!(b.resolve(Some(2)), Some("192k"));
        assert_eq!(b.resolve(Some(6)), Some("320k"));
        assert_eq!(b.resolve(Some(8)), Some("256k")); // falls back to default
        assert_eq!(b.resolve(None), Some("256k"));

        let single = Bitrate::Single("128k".into());
        assert_eq!(single.resolve(Some(6)), Some("128k"));
    }

    #[test]
    fn output_lossless_classification() {
        assert!(output_is_lossless("flac"));
        assert!(output_is_lossless("pcm_s24le"));
        assert!(!output_is_lossless("libopus"));
        assert!(!output_is_lossless("aac"));
    }

    #[test]
    fn flac_encode_needs_no_bitrate_but_opus_does() {
        validate_audio("audio", AudioMode::Encode, Some("flac"), None).unwrap();
        assert!(validate_audio("audio", AudioMode::Encode, Some("libopus"), None).is_err());
        assert!(validate_audio("audio", AudioMode::Encode, None, None).is_err());
    }

    #[test]
    fn resolve_precedence_and_inheritance() {
        let cfg = audio(
            r#"
            mode    = "encode"
            codec   = "libopus"
            bitrate = "192k"
            [lossless]
            codec   = "flac"
            [codec_rules]
            opus = { mode = "copy" }
            "#,
        );
        assert_eq!(cfg.resolve("opus", false).mode, AudioMode::Copy);
        // lossless override keeps inherited mode + bitrate
        let r = cfg.resolve("truehd", true);
        assert_eq!(r.mode, AudioMode::Encode);
        assert_eq!(r.codec, Some("flac"));
        let r = cfg.resolve("eac3", false);
        assert_eq!(r.codec, Some("libopus"));
        assert!(matches!(r.bitrate, Some(Bitrate::Single(s)) if s == "192k"));
    }

    #[test]
    fn target_quality_defaults_and_valid() {
        let c: Config = toml::from_str("encoder = \"svt-av1\"\n[target_quality]\nvmaf = 95").unwrap();
        c.validate().unwrap();
        let tq = c.target_quality.unwrap();
        assert_eq!((tq.min_crf, tq.max_crf, tq.probes, tq.probe_preset), (18, 45, 4, 13));
        assert_eq!((tq.tolerance_under, tq.tolerance_over), (0.5, 2.0));
    }

    #[test]
    fn target_quality_rejects_bad_values() {
        let bad = [
            "encoder = \"svt-av1\"\n[target_quality]\nvmaf = 0",
            "encoder = \"svt-av1\"\n[target_quality]\nvmaf = 101",
            "encoder = \"svt-av1\"\n[target_quality]\nvmaf = 95\nmin_crf = 40\nmax_crf = 30",
            "encoder = \"svt-av1\"\n[target_quality]\nvmaf = 95\nmax_crf = 70",
            "encoder = \"svt-av1\"\n[target_quality]\nvmaf = 95\nprobes = 1",
            "encoder = \"svt-av1\"\n[target_quality]\nvmaf = 95\nprobe_preset = 14",
        ];
        for t in bad {
            let c: Config = toml::from_str(t).unwrap();
            assert!(c.validate().is_err(), "should reject:\n{t}");
        }
    }

    #[test]
    fn target_quality_requires_encode_video() {
        let c: Config = toml::from_str(
            "encoder = \"svt-av1\"\n[avxs]\nvideo = \"copy\"\n[target_quality]\nvmaf = 95",
        )
        .unwrap();
        assert!(c.validate().is_err());
    }
}
