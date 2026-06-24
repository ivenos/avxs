# avxs

![Docker Image Size](https://img.shields.io/docker/image-size/ivenos/avxs)
![Docker Pulls](https://img.shields.io/docker/pulls/ivenos/avxs)

**avxs** is an AV1 encoding service written in Rust, distributed as a Docker image and a self-contained Linux AppImage. Drop video files and an `encode.toml` profile into a folder; avxs picks them up, splits each file into scenes, encodes the chunks in parallel with SVT-AV1, and merges everything back into a finished MKV. It runs as a daemon: point it at an input and an output directory and it keeps watching for new work.

It is built to run unattended. Encodes resume from the last finished chunk after a restart, audio and subtitles are carried over with per-track rules, and every external tool it needs is bundled, so there is nothing to install beside avxs itself.

## Table of contents

- [Features](#features)
- [How it works](#how-it-works)
- [Installation](#installation)
  - [Docker](#docker)
  - [AppImage](#appimage)
  - [Directory layout](#directory-layout)
  - [Environment variables](#environment-variables)
- [Configuration](#configuration)
  - [`encoder`](#encoder)
  - [`[encoder_params]`](#encoder_params)
  - [`[target_quality]`](#target_quality)
  - [`[avxs]`](#avxs)
  - [`[audio]`](#audio)
  - [`[audio.lossless]`](#audiolossless)
  - [`[audio.codec_rules]`](#audiocodec_rules)
  - [`[subtitles]`](#subtitles)
  - [`[scene_detection]`](#scene_detection)
  - [Full example](#full-example)
- [Supported encoders](#supported-encoders)
- [License](#license)

## Features

- **Scene-based parallel encoding** - splits each file into scenes via [av-scenechange](https://github.com/rust-av/av-scenechange) and encodes all chunks in parallel with SVT-AV1. Long scenes are split further so no single chunk holds up the queue.
- **Resumable** - every finished chunk is recorded, so a crash or restart continues where it left off instead of starting over.
- **Target quality (VMAF)** - instead of a fixed CRF, give avxs a target VMAF score. It probes each chunk at a few CRF values, measures VMAF against the source, and encodes at the CRF that hits the target. Ships with libvmaf and the VMAF v1 models built in; the model (1080p or 4K) is picked automatically.
- **HDR passthrough** - auto-detects HDR10, HLG, Dolby Vision and HDR10+ and passes the color metadata (primaries, transfer, mastering display, content light) to the encoder. Dolby Vision and HDR10+ fall back to HDR10 static metadata.
- **Auto-crop** - detects black bars with `cropdetect` and removes them before the encode (crop is applied before scaling, so edges stay clean).
- **Auto-scale** - downscales to a target height with Lanczos while preserving aspect ratio; smaller sources are left untouched.
- **Auto-keyint** - derives `--keyint` from the source frame rate for a ~5 s keyframe interval.
- **Audio control** - copy or re-encode per source codec, with a language whitelist, per-layout bitrates, and automatic lossless handling.
- **Subtitle control** - copy or strip subtitles, with a language whitelist. Chapters are always kept.
- **Self-contained** - ffmpeg, mkvmerge, SvtAv1EncApp, ffmsindex, libffms2 and vmaf/libvmaf are all bundled.

## How it works

For every video next to an `encode.toml`, avxs runs this pipeline:

1. **Index** the source with ffms2 for frame-accurate seeking.
2. **Detect scenes** and cut the file into chunks.
3. **Encode chunks in parallel**, one SVT-AV1 process per worker; the number of workers is derived from CPU cores and free RAM.
4. **Probe for target quality** (optional) - find the per-chunk CRF that hits the VMAF target.
5. **Merge** the chunks, **process audio** (copy or re-encode per track), and **mux** video, audio, subtitles and chapters into the final MKV.
6. **Validate** the output, move the source into `input/processed/`, and clean up temporary files.

Intermediate state (index, scene list, finished chunks, solved CRFs) lives in a hidden `.avxs_<name>/` directory under the output folder, which is what makes encodes resumable. If the encode profile changes between runs (encoder args, crop, scale, bit depth, scene detection), the cached scene list and chunks are discarded and the file is re-encoded.

## Installation

avxs is shipped two ways. Both bundle every tool they need.

### Docker

```yaml
services:
  avxs:
    image: ivenos/avxs:latest
    volumes:
      - ./input:/input
      - ./output:/output
    environment:
      - AVXS_POLL_INTERVAL=60
    restart: unless-stopped
```

The official image presets `AVXS_INPUT_DIR=/input` and `AVXS_OUTPUT_DIR=/output`, which is why the example mounts to those paths. `restart: unless-stopped` is recommended: combined with resume, the daemon recovers cleanly from any interruption.

### AppImage

Linux x86_64 and aarch64. Grab the latest build for your architecture from the [releases page](https://github.com/ivenos/avxs/releases/latest), or:

```sh
ARCH=$(uname -m)
wget "https://github.com/ivenos/avxs/releases/latest/download/avxs-${ARCH}.AppImage"
chmod +x "avxs-${ARCH}.AppImage"
./avxs-${ARCH}.AppImage
```

By default avxs creates `./input/` and `./output/` next to its working directory and watches them. Override with the [environment variables](#environment-variables) below, for example:

```sh
AVXS_INPUT_DIR=/media/in AVXS_OUTPUT_DIR=/media/out RUST_LOG=debug ./avxs-x86_64.AppImage
```

### Directory layout

Inside the input directory, each subfolder is a profile: it holds one `encode.toml` and the video files it applies to.

```
input/
  movies/
    encode.toml          # profile for everything in this folder
    The Movie (2021).mkv
    Another Film.mkv
  anime/
    encode.toml          # a different profile
    Episode 01.mkv
output/
  The Movie (2021).mkv   # finished encodes land here, flat
  Another Film.mkv
input/processed/         # sources are moved here after a successful encode
```

Supported input extensions: `mkv`, `mp4`, `mov`, `avi`, `ts`, `m2ts`, `flv`, `webm`, `m4v`. A file is skipped while it is still being written, and again once its output exists. If an encode fails permanently, a marker is written and the file is skipped until you remove it (the log explains how).

### Environment variables

| Variable | Default | Description |
|---|---|---|
| `AVXS_INPUT_DIR` | `./input` | Input directory to watch |
| `AVXS_OUTPUT_DIR` | `./output` | Output directory for finished files |
| `AVXS_POLL_INTERVAL` | `60` | Directory scan interval in seconds |
| `RUST_LOG` | `info` | Log verbosity. Set to `debug` for verbose output |

## Configuration

Each profile folder contains an `encode.toml`. The only required key is `encoder` (unless you copy the video stream). Everything else has sensible defaults, so a minimal profile is one line:

```toml
encoder = "svt-av1"
```

The sections below document every key. A complete profile is shown in [Full example](#full-example).

### `encoder`

```toml
encoder = "svt-av1"
```

| Value | Description |
|---|---|
| `svt-av1` | SVT-AV1 |
| `svt-av1-hdr` | SVT-AV1-HDR ([juliobbv-p](https://github.com/juliobbv-p/svt-av1-hdr) fork) |

Required unless `avxs.video = "copy"`, in which case the video stream is passed through and no encoder is needed.

### `[encoder_params]`

Passed straight through to SVT-AV1 as `--key value`. Keys are SVT-AV1 long flags without the leading `--`. All keys are optional.

```toml
[encoder_params]
preset = 6
crf    = 28
```

Values may be strings, integers, floats, or booleans (booleans become `1`/`0`).

### `[target_quality]`

Targets a VMAF score per chunk instead of a fixed `crf`. avxs probes each chunk at a few CRF values, measures VMAF against the source, and encodes at the CRF that hits the target. Requires `avxs.video = "encode"`.

```toml
[target_quality]
vmaf = 95
```

| Key | Type | Default | Description |
|---|---|---|---|
| `vmaf` | Float | - | Target VMAF score (required) |
| `min_crf` | Integer | `18` | Lower bound of the CRF search |
| `max_crf` | Integer | `45` | Upper bound of the CRF search |
| `probes` | Integer | `4` | Maximum probe encodes per chunk |
| `probe_preset` | Integer | `13` | SVT-AV1 preset for probe encodes (`13` = fastest) |
| `tolerance_under` | Float | `0.5` | Accept a probe up to this far below the target |
| `tolerance_over` | Float | `2.0` | Accept a probe up to this far above the target |

The VMAF model is selected automatically from the output height: the VMAF v1 1080p model (`vmaf_v1.0.16_3d0h`) below 1440p, the VMAF v1 4K model (`vmaf_v1.0.16_1d5h_2160`) at 1440p and above. libvmaf and both models are bundled, so no setup is needed. VMAF is measured at 10-bit against the source after the same crop and scale as the encode.

Probe encodes use `probe_preset`; the final encode uses the preset from `[encoder_params]`. A probe is accepted when its VMAF falls within `[vmaf - tolerance_under, vmaf + tolerance_over]`. The two tolerances are independent, so the accepted band does not have to be symmetric around the target.

`crf` in `[encoder_params]` is ignored while target quality is active (it is used only as the first probe seed). Solved CRFs are cached, so a resume does not re-probe.

### `[avxs]`

avxs pipeline controls. All flags default to `false` / disabled.

```toml
[avxs]
hdr       = true
crop      = true
keyint    = true
scale     = 1080
bit_depth = 10
keep_temp = false
```

| Key | Type | Default | Description |
|---|---|---|---|
| `video` | `"encode"` \| `"copy"` | `"encode"` | `copy` passes the source video through untouched and only runs the audio and subtitle steps. The video-only options below and `[encoder_params]` are ignored, and `encoder` is not needed. |
| `hdr` | Boolean | `false` | Detect HDR type and pass color metadata (`--color-primaries`, `--transfer-characteristics`, `--matrix-coefficients`, `--chroma-sample-position`, `--content-light`, `--mastering-display`) to the encoder automatically. Works for HDR10, HLG, and Dolby Vision/HDR10+ (fallback to HDR10 metadata). Independent of the encoder binary chosen. |
| `crop` | Boolean | `false` | Detect black bars via `ffmpeg cropdetect` (5 samples at 10/25/40/55/70 % of the runtime, threshold 128 for HDR/10-bit). The detected crop is applied in the Y4M pipe **before** the encoder. Result is cached in the temp directory. |
| `keyint` | Boolean | `false` | Calculate `--keyint` from source FPS for a ~5 s keyframe distance (`round(fps x 5)`). Silently skipped if `keyint` is already set in `[encoder_params]`. |
| `scale` | Integer | - | Maximum output height in pixels. The source is scaled down proportionally with Lanczos if taller than this. If the source (after crop) is already at or below this height, no scaling is applied. Example: `1080` encodes 4K content as 1080p while leaving 720p untouched. Aspect ratio is always preserved. |
| `bit_depth` | `8` \| `10` | - | Force the encoder input bit depth. Omitted passes the source depth through (8-bit stays 8, 10-bit stays 10); sources deeper than 10-bit are clamped to 10-bit, since SVT-AV1 accepts only 8/10-bit input. Set to `10` to convert 8-bit sources to 10-bit before encoding. |
| `keep_temp` | Boolean | `false` | Keep temporary chunks and index files after encoding. |

### `[audio]`

Controls how audio tracks are carried over. This is also the default profile: any track not matched by a more specific rule uses it.

```toml
[audio]
mode    = "encode"
codec   = "libopus"
bitrate = { stereo = "192k", "5.1" = "320k", "7.1" = "512k", default = "192k" }
```

| Key | Type | Default | Description |
|---|---|---|---|
| `mode` | `"copy"` \| `"encode"` | `"copy"` | Copy or re-encode |
| `codec` | String | - | ffmpeg codec name, e.g. `"libopus"`. Required when `mode = "encode"` |
| `bitrate` | String \| table | - | Target bitrate, single value or per-layout table (see below). Required when encoding to a lossy codec |
| `options` | table | `{}` | Extra encoder options, passed as `-<key> <value>`, e.g. `{ compression_level = 12 }` |
| `language_whitelist` | String array | `[]` | Keep only tracks with these language tags (ISO 639-2). Empty = keep all |

The channel count is always taken from the source. FLAC passes the source layout through unchanged; Opus normalizes the layout name to one its encoder accepts but never changes the channel count.

**Bitrate per channel layout.** `bitrate` is either a single string applied to every track, or a table keyed by layout. avxs detects each track's channel count and picks the matching entry; `default` covers anything not listed.

| Channels | Key | Channels | Key |
|---|---|---|---|
| 1 | `mono` | 5 | `5.0` |
| 2 | `stereo` | 6 | `5.1` |
| 3 | `3.0` | 7 | `6.1` |
| 4 | `quad` | 8 | `7.1` |

Lossless codecs (`flac`, `alac`, `wavpack`, `pcm_*`) ignore bitrate, so it may be omitted for them.

**Language whitelist.** When set, only audio tracks whose language tag is in the list are kept. Tracks **without** a language tag are always kept.

```toml
[audio]
language_whitelist = ["deu", "ger"]  # German only
mode = "copy"
```

Common ISO 639-2 codes: `deu`/`ger` (German), `eng` (English), `fra`/`fre` (French), `jpn` (Japanese), `und` (undefined).

**Track titles.** Re-encoded tracks keep their source name with the new codec appended, e.g. `Deutsch Dolby Digital Plus 7.1` becomes `Deutsch Dolby Digital Plus 7.1 (Opus)`. Untitled tracks get the codec name alone; copied tracks keep their name. Before encoding, avxs logs one line per kept track:

```
audio track 0: deu eac3 5.1 (lossy) -> Opus 320k
audio track 1: ger truehd 7.1 (lossless) -> FLAC
```

### `[audio.lossless]`

Override applied to tracks whose **source** is lossless. Any field left unset inherits from `[audio]`, so you usually only set `codec` and maybe `options`.

Lossless is detected from ffmpeg's own codec table (`ffmpeg -codecs`), so every codec ffmpeg flags as lossless is covered. `dts` is special-cased: it counts as lossless only in its Master Audio profile (`DTS-HD MA`), since ffprobe reports lossy and lossless DTS under the same codec name.

```toml
[audio]
mode    = "encode"
codec   = "libopus"
bitrate = { stereo = "192k", "5.1" = "320k", "7.1" = "512k", default = "192k" }

[audio.lossless]
codec   = "flac"
options = { compression_level = 12 }
```

Result: lossless sources become FLAC at maximum compression, everything else becomes Opus.

### `[audio.codec_rules]`

Per source codec override, keyed by the codec name as reported by ffprobe (lowercase). A matching rule has the highest precedence and, like `[audio.lossless]`, inherits any unset field from `[audio]`.

```toml
[audio]
language_whitelist = ["deu", "ger"]
mode = "copy"   # default: copy all codecs not matched by a rule

[audio.codec_rules]
eac3 = { mode = "encode", codec = "libopus", bitrate = "192k" }
opus = { mode = "copy" }   # don't re-encode existing Opus
ac3  = { mode = "encode", codec = "libopus", bitrate = "128k" }
```

**Resolution order for each kept track:**

1. Filter by language whitelist (empty list = no filter).
2. Settings resolve as `codec_rules[codec]` then `[audio.lossless]` (lossless sources only) then `[audio]`. Whichever matches first wins; unset fields inherit from `[audio]`.
3. If no tracks remain after filtering, audio is omitted entirely (warning logged).

Common codec names from ffprobe: `eac3`, `ac3`, `aac`, `truehd`, `dts`, `flac`, `mp3`, `opus`, `vorbis`.

### `[subtitles]`

Controls how subtitle tracks are carried over. By default all subtitles are copied. Chapters are always preserved, regardless of this setting.

```toml
[subtitles]
mode               = "copy"
language_whitelist = ["deu", "eng"]  # German and English only
```

| Key | Type | Default | Description |
|---|---|---|---|
| `mode` | `"copy"` \| `"strip"` | `"copy"` | `copy` keeps subtitle tracks; `strip` removes them all |
| `language_whitelist` | String array | `[]` | Keep only tracks with these language tags (ISO 639-2). Empty = keep all |

When the whitelist is set, only subtitle tracks whose language tag is in the list are kept. Tracks **without** a language tag are always kept. To remove every subtitle:

```toml
[subtitles]
mode = "strip"
```

### `[scene_detection]`

```toml
[scene_detection]
min_scene_len   = 24
extra_split_sec = 10
# extra_split      = 0       # overrides extra_split_sec when > 0
# speed            = "standard"
# downscale_height = 720
```

| Key | Type | Default | Description |
|---|---|---|---|
| `min_scene_len` | Integer | `24` | Minimum chunk length in frames. Cuts closer than this are suppressed. |
| `extra_split_sec` | Integer | `10` | Maximum chunk length in seconds. Longer chunks are split into roughly equal parts. Set to `0` to disable. Ignored when `extra_split` > 0. |
| `extra_split` | Integer | `0` | Maximum chunk length in frames. Overrides `extra_split_sec` when > 0. Set to `0` to use `extra_split_sec` instead. |
| `speed` | `"standard"` \| `"fast"` | `"standard"` | Detection algorithm. `standard` uses SATD-based motion estimation. `fast` uses raw pixel differences, which lowers detection time and accuracy. |
| `downscale_height` | Integer | - | Downscale to this height (e.g. `720`) for scene detection only. Does not affect encoding output. Speeds up detection on high-resolution sources at some accuracy cost. |

### Full example

```toml
encoder = "svt-av1"

[encoder_params]
preset      = 6
crf         = 28
input-depth = 10
lookahead   = 120

[target_quality]
vmaf = 95          # optional: replaces the fixed crf above with a VMAF target

[avxs]
hdr       = true
crop      = true
keyint    = true
scale     = 1080
bit_depth = 10
keep_temp = false

[audio]
language_whitelist = ["deu", "ger"]
mode    = "encode"
codec   = "libopus"
bitrate = { stereo = "192k", "5.1" = "320k", "7.1" = "512k", default = "192k" }

[audio.lossless]
codec   = "flac"
options = { compression_level = 12 }

[audio.codec_rules]
opus = { mode = "copy" }   # don't re-encode existing Opus

[subtitles]
language_whitelist = ["deu", "eng"]

[scene_detection]
min_scene_len   = 24
extra_split_sec = 10
```

## Supported encoders

| `encoder` value | Binary | Version |
|---|---|---|
| `svt-av1` | `SvtAv1EncApp` | [v4.1.0](https://gitlab.com/AOMediaCodec/SVT-AV1) |
| `svt-av1-hdr` | `SvtAv1EncApp-hdr` | [cfb4e17](https://github.com/juliobbv-p/svt-av1-hdr/commit/cfb4e17693ae16945a7fe288d45437243d96c12e) (main) |

## License

[BSL 1.1](LICENSE) - free for personal and non-commercial use.
