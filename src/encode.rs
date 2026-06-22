use anyhow::{bail, Context, Result};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::config::{Config, Encoder};
use crate::ffms2::{Crop, OpenOpts, VideoSource};
use crate::paths::external_bin;
use crate::resume::SceneEntry;

#[derive(Clone)]
pub struct EncodeOptions {
    /// SVT-AV1 HDR args (color-primaries, transfer, etc.)
    pub hdr_args: Vec<String>,
    /// Auto-keyint; skipped if user set "keyint" in encoder_params.
    pub keyint: Option<u32>,
    /// FFMS2 Lanczos target; `crop` coordinates are in this scaled space.
    pub ffms2_target: Option<(u32, u32)>,
    /// Crop applied in the Y4M pipe after scaling.
    pub crop: Option<Crop>,
    /// FPS from ffprobe; FFMS2 reports 0/0 for some exotic containers (e.g. DV) which breaks IVF timestamps.
    pub fps_num: u32,
    pub fps_den: u32,
    /// Forced encoder input bit depth (8 or 10); None = pass source through.
    pub target_bit_depth: Option<u8>,
}

pub fn encode_chunk(
    source_file: PathBuf,
    index_file: PathBuf,
    scene: SceneEntry,
    output_path: PathBuf,
    config: &Config,
    opts: &EncodeOptions,
) -> Result<u64> {
    let encoder = config.encoder.context("encoder is required when video is encoded")?;
    let encoder_name = encoder_binary(encoder);
    let encoder_bin = external_bin(encoder_name);
    let encoder_args = build_encoder_args(config, &output_path, opts)?;

    let mut child = std::process::Command::new(&encoder_bin)
        .args(&encoder_args)
        .args(["--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start encoder '{encoder_name}'"))?;

    let mut stdin = BufWriter::with_capacity(256 * 1024, child.stdin.take().expect("encoder stdin unavailable"));

    let mut vs = VideoSource::open(
        &source_file,
        &index_file,
        OpenOpts {
            target_size:      opts.ffms2_target,
            target_bit_depth: opts.target_bit_depth,
        },
    )
    .context("open FFMS2 VideoSource")?;
    // Override FFMS2 fps with ffprobe value (FFMS2 returns 0/0 for some containers, corrupting IVF timestamps).
    vs.info.fps_num = opts.fps_num;
    vs.info.fps_den = opts.fps_den;
    if let Err(e) =
        vs.write_y4m_range(&mut stdin, scene.start_frame, scene.end_frame, opts.crop)
    {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return Err(e.context("write Y4M frames to encoder"));
    }

    drop(stdin);

    let out = child.wait_with_output().context("wait for encoder")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("encoder failed (chunk {:05}):\n{stderr}", scene.index + 1);
    }

    let meta = std::fs::metadata(&output_path)
        .with_context(|| format!("chunk output not found: {}", output_path.display()))?;
    if meta.len() == 0 {
        bail!("encoder produced empty file: {}", output_path.display());
    }

    Ok(meta.len())
}

fn encoder_binary(enc: Encoder) -> &'static str {
    match enc {
        Encoder::SvtAv1    => "SvtAv1EncApp",
        Encoder::SvtAv1Hdr => "SvtAv1EncApp-hdr",
    }
}

fn build_encoder_args(config: &Config, output_path: &Path, opts: &EncodeOptions) -> Result<Vec<String>> {
    let out = output_path.to_str()
        .with_context(|| format!("non-UTF8 output path: {}", output_path.display()))?;

    let mut args = vec!["-b".to_string(), out.to_string()];
    args.extend(merged_encoder_args(config, opts));
    Ok(args)
}

/// Encoder args from config, merged with auto-HDR and auto-keyint.
/// Auto-args are skipped when the same key is already in `encoder_params` (user override wins).
pub fn merged_encoder_args(config: &Config, opts: &EncodeOptions) -> Vec<String> {
    let mut args = config.encoder_args();

    debug_assert_eq!(opts.hdr_args.len() % 2, 0, "hdr_args must contain flag-value pairs");
    for pair in opts.hdr_args.chunks(2) {
        if let [flag, value] = pair {
            let key = flag.trim_start_matches('-');
            if config.encoder_params.contains_key(key) {
                tracing::debug!("auto-HDR: skipping {flag} - overridden by encoder_params");
            } else {
                args.push(flag.clone());
                args.push(value.clone());
            }
        }
    }

    if let Some(keyint) = opts.keyint
        && !config.encoder_params.contains_key("keyint")
    {
        args.extend_from_slice(&["--keyint".into(), keyint.to_string()]);
    }

    args
}

pub async fn validate_output(path: &Path) -> Result<()> {
    let out = tokio::process::Command::new(external_bin("ffprobe"))
        .args(["-v", "error", "-i"])
        .arg(path)
        .output()
        .await
        .context("start ffprobe for output validation")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("output file is invalid: {stderr}");
    }
    Ok(())
}

pub async fn concat_chunks(
    chunk_paths: &[PathBuf],
    output_path: &Path,
    list_dir: &Path,
) -> Result<()> {
    let list_path = list_dir.join("concat_list.txt");

    {
        let mut f = std::fs::File::create(&list_path).context("create concat_list.txt")?;
        for p in chunk_paths {
            let path_str = p.to_str()
                .with_context(|| format!("non-UTF8 chunk path: {}", p.display()))?;
            // ffmpeg concat parser: backslash-escape special chars (not shell semantics).
            let escaped = path_str
                .replace('\\', "\\\\")
                .replace(' ',  "\\ ")
                .replace('\'', "\\'");
            writeln!(f, "file {escaped}").context("write concat_list.txt")?;
        }
    }

    let out = tokio::process::Command::new(external_bin("ffmpeg"))
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list_path)
        .args(["-c:v", "copy", "-map_metadata", "-1", "-an", "-sn"])
        .arg(output_path)
        .output()
        .await
        .context("start ffmpeg concat")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg concat failed:\n{stderr}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_encoder_args_includes_params() {
        use crate::config::{AudioConfig, AvxsConfig, SceneDetectionConfig};
        use std::collections::HashMap;

        let mut params = HashMap::new();
        params.insert("crf".to_string(), toml::Value::Integer(28));
        params.insert("preset".to_string(), toml::Value::Integer(6));

        let config = Config {
            encoder: Some(Encoder::SvtAv1),
            encoder_params: params,
            avxs: AvxsConfig::default(),
            audio: AudioConfig::default(),
            subtitles: crate::config::SubtitleConfig::default(),
            scene_detection: SceneDetectionConfig::default(),
        };

        let opts = EncodeOptions {
            hdr_args: Vec::new(),
            keyint: None,
            ffms2_target: None,
            crop: None,
            fps_num: 24,
            fps_den: 1,
            target_bit_depth: None,
        };

        let out = PathBuf::from("/tmp/chunk.ivf");
        let args = build_encoder_args(&config, &out, &opts).unwrap();

        assert!(args.contains(&"-b".to_string()));
        assert!(args.contains(&"--crf".to_string()));
        assert!(args.contains(&"28".to_string()));
        assert!(args.contains(&"--preset".to_string()));
        assert!(args.contains(&"6".to_string()));
    }

    #[test]
    fn auto_keyint_skipped_when_manual() {
        use crate::config::{AudioConfig, AvxsConfig, SceneDetectionConfig};
        use std::collections::HashMap;

        let mut params = HashMap::new();
        params.insert("keyint".to_string(), toml::Value::Integer(240));

        let config = Config {
            encoder: Some(Encoder::SvtAv1),
            encoder_params: params,
            avxs: AvxsConfig::default(),
            audio: AudioConfig::default(),
            subtitles: crate::config::SubtitleConfig::default(),
            scene_detection: SceneDetectionConfig::default(),
        };

        let opts = EncodeOptions {
            hdr_args: Vec::new(),
            keyint: Some(120), // would be auto-keyint
            ffms2_target: None,
            crop: None,
            fps_num: 24,
            fps_den: 1,
            target_bit_depth: None,
        };

        let out = PathBuf::from("/tmp/chunk.ivf");
        let args = build_encoder_args(&config, &out, &opts).unwrap();

        // Manual keyint=240 should be present, auto 120 should not appear
        let keyint_pos = args.iter().position(|a| a == "--keyint").unwrap();
        assert_eq!(args[keyint_pos + 1], "240");
        assert_eq!(args.iter().filter(|a| *a == "--keyint").count(), 1);
    }

    #[test]
    fn auto_keyint_injected_when_not_manual() {
        use crate::config::{AudioConfig, AvxsConfig, SceneDetectionConfig};
        use std::collections::HashMap;

        let config = Config {
            encoder: Some(Encoder::SvtAv1),
            encoder_params: HashMap::new(),
            avxs: AvxsConfig::default(),
            audio: AudioConfig::default(),
            subtitles: crate::config::SubtitleConfig::default(),
            scene_detection: SceneDetectionConfig::default(),
        };

        let opts = EncodeOptions {
            hdr_args: Vec::new(),
            keyint: Some(120),
            ffms2_target: None,
            crop: None,
            fps_num: 24,
            fps_den: 1,
            target_bit_depth: None,
        };

        let out = PathBuf::from("/tmp/chunk.ivf");
        let args = build_encoder_args(&config, &out, &opts).unwrap();

        let keyint_pos = args.iter().position(|a| a == "--keyint").unwrap();
        assert_eq!(args[keyint_pos + 1], "120");
    }

    #[test]
    fn auto_hdr_skipped_when_manual_override() {
        use crate::config::{AudioConfig, AvxsConfig, SceneDetectionConfig};
        use std::collections::HashMap;

        let mut params = HashMap::new();
        // User pinned color-primaries; auto-HDR must not override.
        params.insert("color-primaries".to_string(), toml::Value::Integer(1));

        let config = Config {
            encoder: Some(Encoder::SvtAv1),
            encoder_params: params,
            avxs: AvxsConfig::default(),
            audio: AudioConfig::default(),
            subtitles: crate::config::SubtitleConfig::default(),
            scene_detection: SceneDetectionConfig::default(),
        };

        let opts = EncodeOptions {
            hdr_args: vec![
                "--color-primaries".into(), "9".into(),
                "--transfer-characteristics".into(), "16".into(),
            ],
            keyint: None,
            ffms2_target: None,
            crop: None,
            fps_num: 24,
            fps_den: 1,
            target_bit_depth: None,
        };

        let args = merged_encoder_args(&config, &opts);
        // User's value 1 wins for color-primaries.
        let pos = args.iter().position(|a| a == "--color-primaries").unwrap();
        assert_eq!(args[pos + 1], "1");
        assert_eq!(args.iter().filter(|a| *a == "--color-primaries").count(), 1);
        // Non-overridden auto-HDR arg still injected.
        assert!(args.windows(2).any(|w| w[0] == "--transfer-characteristics" && w[1] == "16"));
    }
}
