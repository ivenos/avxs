# encode.toml Reference

Each input directory can contain an `encode.toml` that defines the encoding profile for all videos in that folder.

---

## Required fields

```toml
encoder = "svt-av1"
```

| Value | Description |
|---|---|
| `svt-av1` | SVT-AV1 (default) |
| `svt-av1-hdr` | SVT-AV1-HDR (juliobbv-p fork) |

---

## `[encoder_params]`

Encoder parameters are passed directly as `--key value` to SVT-AV1. All keys are optional.

```toml
[encoder_params]
preset = 6
crf    = 28
```

Keys correspond to SVT-AV1 long flags without `--`. Values can be strings, integers, floats, or booleans (booleans are passed as `1`/`0`).

---

## `[avxs]`

avxs pipeline controls. All flags default to `false` / disabled.

| Key | Type | Default | Description |
|---|---|---|---|
| `hdr` | Boolean | `false` | Detect HDR type and pass color metadata (`--color-primaries`, `--transfer-characteristics`, `--matrix-coefficients`, `--chroma-sample-position`, `--content-light`, `--mastering-display`) to the encoder automatically. Works for HDR10, HLG, and Dolby Vision/HDR10+ (fallback to HDR10 metadata). Independent of the encoder binary chosen. |
| `crop` | Boolean | `false` | Detect black bars via `ffmpeg cropdetect` (5 samples at 10/25/40/55/70 % of the runtime, threshold 128 for HDR/10-bit). The detected crop is applied in the Y4M pipe **before** the encoder. Result is cached in `crop.cache` inside the temp directory. |
| `keyint` | Boolean | `false` | Calculate `--keyint` from source FPS for a ~5 s keyframe distance (`round(fps × 5)`). Silently skipped if `keyint` is already set in `[encoder_params]`. |
| `scale` | Integer | - | Maximum output height in pixels. The source is scaled down proportionally using Lanczos resampling if taller than this value. If the source (after crop) is already at or below this height, no scaling is applied. Example: `1080` encodes 4K content as 1080p while leaving 720p content untouched. Aspect ratio is always preserved. |
| `keep_temp` | Boolean | `false` | Keep temporary chunks and index files after encoding. |

```toml
[avxs]
hdr       = true
crop      = true
keyint    = true
scale     = 1080
keep_temp = false
```

---

## `[audio]`

Controls how audio tracks are carried over from the source file.

| Key | Type | Default | Description |
|---|---|---|---|
| `mode` | `"copy"` \| `"encode"` | `"copy"` | Global default for all audio tracks |
| `codec` | String | - | FFmpeg codec name, e.g. `"libopus"` - required when `mode = "encode"` |
| `bitrate` | String | - | Target bitrate, e.g. `"192k"` - required when `mode = "encode"` |
| `language_whitelist` | String array | `[]` | Keep only tracks with these language tags (ISO 639-2). Empty = keep all |

### Language whitelist

When `language_whitelist` is set, only audio tracks whose language tag is in the list are kept. Tracks **without a language tag** are always kept.

```toml
[audio]
language_whitelist = ["deu", "ger"]  # German only
mode = "copy"
```

Common ISO 639-2 codes: `deu`/`ger` (German), `eng` (English), `fra`/`fre` (French), `jpn` (Japanese), `und` (undefined).

### Global encode

Re-encode all audio tracks to a different codec:

```toml
[audio]
mode    = "encode"
codec   = "libopus"
bitrate = "192k"
```

---

## `[audio.codec_rules]`

Allows different treatment per source codec. The key is the codec name as reported by `ffprobe` (lowercase).

When a matching rule exists for a track's codec, it takes precedence over the global `[audio]` default.

| Key in rule | Type | Description |
|---|---|---|
| `mode` | `"copy"` \| `"encode"` | Required |
| `codec` | String | FFmpeg codec name - required when `mode = "encode"` |
| `bitrate` | String | Target bitrate - required when `mode = "encode"` |

```toml
[audio]
language_whitelist = ["deu", "ger"]
mode = "copy"   # default: copy all codecs not matched by a rule

[audio.codec_rules]
eac3   = { mode = "encode", codec = "libopus", bitrate = "192k" }
truehd = { mode = "encode", codec = "libopus", bitrate = "256k" }
dts    = { mode = "encode", codec = "libopus", bitrate = "192k" }
ac3    = { mode = "encode", codec = "libopus", bitrate = "128k" }
```

**Processing order:**
1. Filter by language whitelist (empty list = no filter)
2. For each kept track: apply the matching `codec_rules` entry if one exists, otherwise fall back to the global default
3. If no tracks remain after filtering, audio is omitted entirely (warning logged)

Common codec names reported by ffprobe: `eac3`, `ac3`, `aac`, `truehd`, `dts`, `flac`, `mp3`, `opus`, `vorbis`.

---

## `[subtitles]`

Controls how subtitle tracks are carried over from the source file. By default all subtitle tracks are copied.

| Key | Type | Default | Description |
|---|---|---|---|
| `mode` | `"copy"` \| `"strip"` | `"copy"` | `copy` keeps subtitle tracks; `strip` removes all subtitles from the output |
| `language_whitelist` | String array | `[]` | Keep only tracks with these language tags (ISO 639-2). Empty = keep all |

When `language_whitelist` is set, only subtitle tracks whose language tag is in the list are kept. Tracks **without a language tag** are always kept.

```toml
[subtitles]
mode               = "copy"
language_whitelist = ["deu", "eng"]  # German and English only
```

Strip all subtitles entirely:

```toml
[subtitles]
mode = "strip"
```

Chapters are always preserved regardless of the subtitle mode.

---

## `[scene_detection]`

| Key | Type | Default | Description |
|---|---|---|---|
| `min_scene_len` | Integer | `24` | Minimum chunk length in frames. Cuts closer together than this are suppressed. |
| `extra_split_sec` | Integer | `10` | Maximum chunk length in seconds. Chunks longer than this are split into roughly equal parts. Set to `0` to disable. Ignored when `extra_split` > 0. |
| `extra_split` | Integer | `0` | Maximum chunk length in frames. Overrides `extra_split_sec` when > 0. Set to `0` to use `extra_split_sec` instead. |
| `speed` | `"standard"` \| `"fast"` | `"standard"` | Detection algorithm. `standard` uses SATD-based motion estimation for accurate results. `fast` uses raw pixel differences and is 2–3× faster but less accurate, recommended for high resolutions. |
| `downscale_height` | Integer | - | Downscale to this height (e.g. `720`) for scene detection only. Does not affect encoding output. Reduces detection time for high-resolution sources at the cost of some accuracy. |

```toml
[scene_detection]
min_scene_len    = 24
extra_split_sec  = 10
# extra_split    = 0      # overrides extra_split_sec when > 0
# speed          = "standard"
# downscale_height = 720
```

---

## Full example

```toml
encoder = "svt-av1"

[encoder_params]
preset      = 6
crf         = 28
input-depth = 10
lookahead   = 120

[avxs]
hdr       = true
crop      = true
keyint    = true
scale     = 1080
keep_temp = false

[audio]
language_whitelist = ["deu", "ger"]
mode = "copy"

[audio.codec_rules]
eac3   = { mode = "encode", codec = "libopus", bitrate = "192k" }
truehd = { mode = "encode", codec = "libopus", bitrate = "256k" }
dts    = { mode = "encode", codec = "libopus", bitrate = "192k" }

[subtitles]
language_whitelist = ["deu", "eng"]

[scene_detection]
min_scene_len   = 24
extra_split_sec = 10
```
