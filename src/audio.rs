use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::config::{AudioConfig, AudioMode, layout_name, output_is_lossless, toml_value_to_arg};
use crate::ext::external_bin;

// Only libopus-native layouts, so aformat remaps e.g. 5.1(side) -> 5.1 without
// dropping channels. Other codecs keep the source layout.
const OPUS_CHANNEL_LAYOUTS: &str =
    "aformat=channel_layouts=7.1|6.1|5.1|5.0|quad|3.0|stereo|mono";

#[derive(Deserialize)]
struct FfprobeOutput {
    streams: Vec<FfprobeStream>,
}

#[derive(Deserialize)]
struct FfprobeStream {
    codec_name: String,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    channels: Option<u32>,
    #[serde(default)]
    tags: FfprobeTags,
}

#[derive(Deserialize, Default)]
struct FfprobeTags {
    language: Option<String>,
    title: Option<String>,
}

struct AudioTrack {
    audio_index: usize,
    codec_name: String,
    profile: Option<String>,
    channels: Option<u32>,
    language: Option<String>,
    title: Option<String>,
}

/// Audio codec names ffmpeg flags as lossless, queried once from `ffmpeg -codecs`.
static LOSSLESS_CODECS: OnceCell<HashSet<String>> = OnceCell::const_new();

async fn lossless_codecs() -> &'static HashSet<String> {
    LOSSLESS_CODECS
        .get_or_init(|| async {
            match probe_lossless_codecs().await {
                Ok(set) => set,
                Err(e) => {
                    tracing::warn!("ffmpeg -codecs query failed ({e:#}); using built-in lossless list");
                    fallback_lossless_codecs()
                }
            }
        })
        .await
}

async fn probe_lossless_codecs() -> Result<HashSet<String>> {
    let out = Command::new(external_bin("ffmpeg"))
        .args(["-hide_banner", "-codecs"])
        .output()
        .await
        .context("run ffmpeg -codecs")?;
    if !out.status.success() {
        bail!("ffmpeg -codecs exited with failure");
    }
    Ok(parse_lossless_codecs(&String::from_utf8_lossy(&out.stdout)))
}

/// Audio codecs flagged lossless (flag 3 = A, flag 6 = S) in `ffmpeg -codecs`.
fn parse_lossless_codecs(stdout: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let (Some(flags), Some(name)) = (fields.next(), fields.next()) else { continue };
        let f = flags.as_bytes();
        if f.len() >= 6 && f[2] == b'A' && f[5] == b'S' {
            set.insert(name.to_string());
        }
    }
    set
}

fn fallback_lossless_codecs() -> HashSet<String> {
    [
        "truehd", "mlp", "flac", "alac", "ape", "tta", "wavpack", "tak",
        "shorten", "ralf", "wmalossless", "mp4als", "als", "dts",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// PCM is always lossless; DTS only in its Master Audio profile.
fn is_lossless(codec_name: &str, profile: Option<&str>, lossless: &HashSet<String>) -> bool {
    if codec_name == "dts" {
        return profile.is_some_and(|p| p.contains("MA"));
    }
    codec_name.starts_with("pcm_") || lossless.contains(codec_name)
}

fn codec_display(codec: &str) -> &str {
    match codec {
        "libopus" | "opus" => "Opus",
        "flac" => "FLAC",
        "aac" | "libfdk_aac" => "AAC",
        "ac3" => "AC3",
        "eac3" => "E-AC-3",
        "libmp3lame" | "mp3" => "MP3",
        "alac" => "ALAC",
        "libvorbis" | "vorbis" => "Vorbis",
        other => other,
    }
}

#[derive(Deserialize, Default)]
struct FfprobeDisposition {
    #[serde(default)] default: i32,
    #[serde(default)] forced: i32,
    #[serde(default)] hearing_impaired: i32,
    #[serde(default)] visual_impaired: i32,
    #[serde(default)] original: i32,
    #[serde(default)] comment: i32,
}

impl FfprobeDisposition {
    fn to_mkvmerge_flags(&self, tid: usize) -> Vec<String> {
        let t = tid.to_string();
        let yn = |v: i32| if v != 0 { "yes" } else { "no" };
        let mut flags = vec![
            "--default-track-flag".into(), format!("{t}:{}", yn(self.default)),
        ];
        let extras: &[(&str, i32)] = &[
            ("--forced-display-flag",    self.forced),
            ("--hearing-impaired-flag",  self.hearing_impaired),
            ("--visual-impaired-flag",   self.visual_impaired),
            ("--original-flag",          self.original),
            ("--commentary-flag",        self.comment),
        ];
        for (flag, val) in extras {
            if *val != 0 {
                flags.extend([(*flag).to_string(), format!("{t}:yes")]);
            }
        }
        flags
    }
}

#[derive(Deserialize)]
struct FfprobeDispStream {
    #[serde(default)]
    disposition: FfprobeDisposition,
}

#[derive(Deserialize)]
struct FfprobeDispOutput {
    #[serde(default)]
    streams: Vec<FfprobeDispStream>,
}

async fn probe_dispositions(path: &Path, stream_spec: &str) -> Vec<FfprobeDisposition> {
    match crate::ext::ffprobe_json::<FfprobeDispOutput>(
        &["-v", "error", "-select_streams", stream_spec,
          "-show_entries", "stream=disposition", "-of", "json"],
        path,
    )
    .await
    {
        Ok(p) => p.streams.into_iter().map(|s| s.disposition).collect(),
        Err(e) => {
            tracing::warn!("ffprobe disposition probe failed for {}: {e:#}", path.display());
            vec![]
        }
    }
}

async fn probe_audio_tracks(source_file: &Path) -> Result<Vec<AudioTrack>> {
    let parsed: FfprobeOutput = crate::ext::ffprobe_json(
        &["-v", "error", "-select_streams", "a",
          "-show_entries", "stream=codec_name,profile,channels:stream_tags=language,title",
          "-of", "json"],
        source_file,
    )
    .await?;

    Ok(parsed
        .streams
        .into_iter()
        .enumerate()
        .map(|(i, s)| AudioTrack {
            audio_index: i,
            codec_name: s.codec_name,
            profile: s.profile,
            channels: s.channels,
            language: s.tags.language,
            title: s.tags.title,
        })
        .collect())
}

fn track_passes_whitelist(track: &AudioTrack, whitelist: &[String]) -> bool {
    if whitelist.is_empty() {
        return true;
    }
    match &track.language {
        // no language tag, always keep
        None => true,
        Some(lang) => whitelist.iter().any(|w| w == lang),
    }
}

enum Action {
    Copy,
    Encode {
        codec: String,
        bitrate: Option<String>,
        options: Vec<(String, String)>,
    },
}

struct PlannedTrack {
    audio_index: usize,
    codec_name: String,
    channels: Option<u32>,
    language: Option<String>,
    title: Option<String>,
    lossless: bool,
    action: Action,
}

/// Per-track audio decisions, built before the encode and run by `process_plan`.
pub struct AudioPlan {
    tracks: Vec<PlannedTrack>,
}

pub async fn plan(source_file: &Path, config: &AudioConfig) -> Result<AudioPlan> {
    let tracks = probe_audio_tracks(source_file).await?;
    if tracks.is_empty() {
        return Ok(AudioPlan { tracks: vec![] });
    }

    let kept: Vec<AudioTrack> = tracks
        .into_iter()
        .filter(|t| track_passes_whitelist(t, &config.language_whitelist))
        .collect();

    if kept.is_empty() {
        tracing::warn!(
            "no audio tracks match language whitelist {:?} - audio omitted",
            config.language_whitelist
        );
        return Ok(AudioPlan { tracks: vec![] });
    }

    let lossless_set = lossless_codecs().await;
    let mut planned = Vec::with_capacity(kept.len());
    for track in kept {
        let lossless = is_lossless(&track.codec_name, track.profile.as_deref(), lossless_set);
        let r = config.resolve(&track.codec_name, lossless);
        let action = match r.mode {
            AudioMode::Copy => Action::Copy,
            AudioMode::Encode => {
                let codec = r.codec.ok_or_else(|| anyhow::anyhow!(
                    "audio track {}: codec is required when mode = encode", track.audio_index
                ))?;
                // lossless ignores bitrate
                let bitrate = if output_is_lossless(codec) {
                    None
                } else {
                    let b = r.bitrate.and_then(|b| b.resolve(track.channels)).map(str::to_owned);
                    if b.is_none() {
                        tracing::warn!(
                            "audio track {}: no bitrate for {} channels, using encoder default",
                            track.audio_index,
                            track.channels.map_or_else(|| "?".into(), |c| c.to_string()),
                        );
                    }
                    b
                };
                let mut options: Vec<(String, String)> = r.options
                    .iter()
                    .map(|(k, v)| (k.clone(), toml_value_to_arg(v)))
                    .collect();
                options.sort();
                Action::Encode { codec: codec.to_owned(), bitrate, options }
            }
        };
        planned.push(PlannedTrack {
            audio_index: track.audio_index,
            codec_name: track.codec_name,
            channels: track.channels,
            language: track.language,
            title: track.title,
            lossless,
            action,
        });
    }

    Ok(AudioPlan { tracks: planned })
}

impl AudioPlan {
    pub fn summary_lines(&self) -> Vec<String> {
        if self.tracks.is_empty() {
            return vec!["no audio tracks".to_string()];
        }
        self.tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let lang = t.language.as_deref().unwrap_or("und");
                let layout = t.channels.map_or("?", layout_name);
                let kind = if t.lossless { "lossless" } else { "lossy" };
                let action = match &t.action {
                    Action::Copy => "copy".to_string(),
                    Action::Encode { codec, bitrate, .. } => match bitrate {
                        Some(b) => format!("{} {b}", codec_display(codec)),
                        None => codec_display(codec).to_string(),
                    },
                };
                format!("track {i}: {lang} {} {layout} ({kind}) -> {action}", t.codec_name)
            })
            .collect()
    }
}

pub async fn process_plan(source_file: &Path, temp_dir: &Path, plan: &AudioPlan) -> Result<PathBuf> {
    let audio_path = temp_dir.join("audio.mkv");
    if plan.tracks.is_empty() {
        return Ok(audio_path);
    }

    let mut cmd = Command::new(external_bin("ffmpeg"));
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .arg("-i")
        .arg(source_file)
        .args(["-vn", "-sn"]);

    for t in &plan.tracks {
        cmd.args(["-map", &format!("0:a:{}", t.audio_index)]);
    }

    for (out_idx, t) in plan.tracks.iter().enumerate() {
        match &t.action {
            Action::Copy => {
                cmd.args([format!("-c:a:{out_idx}"), "copy".into()]);
            }
            Action::Encode { codec, bitrate, options } => {
                if codec.contains("opus") {
                    cmd.args([format!("-filter:a:{out_idx}"), OPUS_CHANNEL_LAYOUTS.into()]);
                }
                cmd.args([format!("-c:a:{out_idx}"), codec.clone()]);
                if let Some(b) = bitrate {
                    cmd.args([format!("-b:a:{out_idx}"), b.clone()]);
                }
                for (k, v) in options {
                    cmd.args([format!("-{k}:a:{out_idx}"), v.clone()]);
                }
                // keep source name, append codec marker
                let marker = codec_display(codec);
                let name = match t.title.as_deref() {
                    Some(title) if !title.is_empty() => format!("{title} ({marker})"),
                    _ => marker.to_string(),
                };
                cmd.args([format!("-metadata:s:a:{out_idx}"), format!("title={name}")]);
            }
        }
    }

    cmd.arg(&audio_path);

    let out = cmd.output().await.context("start ffmpeg audio extraction")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg audio extraction failed:\n{stderr}");
    }

    Ok(audio_path)
}

pub async fn mux_final(
    video_path: &Path,
    audio_path: &Path,
    source_file: &Path,
    output_path: &Path,
    subtitle: &crate::subtitle::SubtitleSelection,
) -> Result<()> {
    let has_audio = audio_path.exists()
        && std::fs::metadata(audio_path).map(|m| m.len()).unwrap_or(0) > 0;

    // Video dispositions come from source (encoded file has none); audio dispositions are already in audio.mkv.
    let video_disps = probe_dispositions(source_file, "v").await;

    let mut cmd = Command::new(external_bin("mkvmerge"));
    cmd.arg("-o").arg(output_path);

    // Video track: apply dispositions from source, strip everything else
    if let Some(d) = video_disps.first() {
        cmd.args(d.to_mkvmerge_flags(0));
    }
    cmd.args(["--no-audio", "--no-subtitles", "--no-chapters",
              "--no-global-tags", "--no-track-tags"]);
    cmd.arg(video_path);

    // Audio: dispositions already in audio.mkv
    if has_audio {
        cmd.args(["--no-video", "--no-subtitles", "--no-chapters",
                  "--no-global-tags", "--no-track-tags"]);
        cmd.arg(audio_path);
    }

    // Source: subtitles + chapters only, no tags
    cmd.args(["--no-video", "--no-audio", "--no-global-tags", "--no-track-tags"]);
    match subtitle {
        crate::subtitle::SubtitleSelection::Strip => {
            cmd.arg("--no-subtitles");
        }
        crate::subtitle::SubtitleSelection::All => {}
        crate::subtitle::SubtitleSelection::Indices(indices) => {
            if indices.is_empty() {
                cmd.arg("--no-subtitles");
            } else {
                let track_ids = crate::subtitle::probe_track_ids(source_file, indices).await?;
                if track_ids.is_empty() {
                    cmd.arg("--no-subtitles");
                } else {
                    let tracks = track_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(",");
                    cmd.args(["--subtitle-tracks", &tracks]);
                }
            }
        }
    }
    cmd.arg(source_file);

    let out = cmd.output().await.context("start mkvmerge")?;
    // mkvmerge exits 1 for warnings (non-fatal), 2+ for errors
    if out.status.code().unwrap_or(2) >= 2 {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("mkvmerge failed:\n{stderr}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn sample_set() -> HashSet<String> {
        ["truehd", "flac", "alac", "dts", "wavpack"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn lossless_by_codec_name() {
        let set = sample_set();
        assert!(is_lossless("truehd", None, &set));
        assert!(is_lossless("flac", None, &set));
        assert!(is_lossless("pcm_s24le", None, &set)); // pcm always lossless
        assert!(!is_lossless("eac3", None, &set));
        assert!(!is_lossless("aac", None, &set));
    }

    #[test]
    fn dts_lossless_only_for_master_audio() {
        let set = sample_set();
        assert!(is_lossless("dts", Some("DTS-HD MA"), &set));
        assert!(!is_lossless("dts", Some("DTS-HD HRA"), &set));
        assert!(!is_lossless("dts", Some("DTS"), &set));
        assert!(!is_lossless("dts", None, &set));
    }

    #[test]
    fn parse_codecs_picks_audio_lossless() {
        let sample = "\
 DEA..S flac    FLAC (Free Lossless Audio Codec)
 DEA.L. aac     AAC (Advanced Audio Coding)
 DEAI.S truehd  TrueHD
 DEV..S ffv1    FFV1 (video, lossless)
 D.A.LS dts     DCA (DTS Coherent Acoustics)
";
        let set = parse_lossless_codecs(sample);
        assert!(set.contains("flac"));
        assert!(set.contains("truehd"));
        assert!(set.contains("dts"));
        assert!(!set.contains("aac"));
        assert!(!set.contains("ffv1"));
    }
}
