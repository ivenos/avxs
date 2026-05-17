use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub encoder: Encoder,
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
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Encoder {
    SvtAv1,
    SvtAv1Hdr,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct AvxsConfig {
    #[serde(default)]
    pub hdr: bool,
    #[serde(default)]
    pub crop: bool,
    #[serde(default)]
    pub keyint: bool,
    pub scale: Option<u32>,
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
    pub bitrate: Option<String>,
    #[serde(default)]
    pub language_whitelist: Vec<String>,
    #[serde(default)]
    pub codec_rules: HashMap<String, AudioCodecRule>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AudioCodecRule {
    pub mode: AudioMode,
    pub codec: Option<String>,
    pub bitrate: Option<String>,
}

#[derive(Debug, Deserialize, Default, PartialEq, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    #[default]
    Copy,
    Encode,
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
        if self.audio.mode == AudioMode::Encode {
            if self.audio.codec.is_none() {
                bail!("audio.codec required when audio.mode = encode");
            }
            if self.audio.bitrate.is_none() {
                bail!("audio.bitrate required when audio.mode = encode");
            }
        }
        for (source_codec, rule) in &self.audio.codec_rules {
            if rule.mode == AudioMode::Encode {
                if rule.codec.is_none() {
                    bail!("audio.codec_rules.{source_codec}: codec required when mode = encode");
                }
                if rule.bitrate.is_none() {
                    bail!("audio.codec_rules.{source_codec}: bitrate required when mode = encode");
                }
            }
        }
        Ok(())
    }

    pub fn encoder_args(&self) -> Vec<String> {
        let mut entries: Vec<_> = self.encoder_params.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        let mut args = Vec::with_capacity(entries.len() * 2);
        for (k, v) in entries {
            args.push(format!("--{k}"));
            args.push(match v {
                toml::Value::String(s)  => s.clone(),
                toml::Value::Integer(i) => i.to_string(),
                toml::Value::Float(f)   => f.to_string(),
                toml::Value::Boolean(b) => if *b { "1".into() } else { "0".into() },
                other                   => other.to_string(),
            });
        }
        args
    }
}
