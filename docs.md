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

Required unless `avxs.video = "copy"`, where the video is not encoded.

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

## `[target_quality]`

Targets a VMAF score per chunk instead of a fixed `crf`. avxs probes each chunk at a few CRF values, measures VMAF against the source, and encodes at the CRF that hits the target. Requires `avxs.video = "encode"`.

| Key | Type | Default | Description |
|---|---|---|---|
| `vmaf` | Float | - | Target VMAF score (required) |
| `min_crf` | Integer | `18` | Lower bound of the CRF search |
| `max_crf` | Integer | `45` | Upper bound of the CRF search |
| `probes` | Integer | `4` | Maximum probe encodes per chunk |
| `probe_preset` | Integer | `13` | SVT-AV1 preset for probe encodes (`13` = fastest) |
| `tolerance_under` | Float | `0.5` | Accept a probe up to this far below the target |
| `tolerance_over` | Float | `2.0` | Accept a probe up to this far above the target |

```toml
[target_quality]
vmaf = 95
```

The VMAF model is selected automatically from the output height: VMAF v1 1080p (`vmaf_v1.0.16_3d0h`) below 1440p, VMAF v1 4K (`vmaf_v1.0.16_1d5h_2160`) at 1440p and above. avxs bundles the `vmaf` tool (libvmaf 3.2.0 with the v1 models built in), so no extra setup is needed. VMAF is measured at 10-bit against the source after the same crop and scale as the encode.

Probes use the fastest preset by default, so the final encode (at your `[encoder_params]` preset) lands at or slightly above the target. `tolerance_under` and `tolerance_over` are asymmetric on purpose: overshooting quality is cheaper to accept than undershooting it.

`crf` in `[encoder_params]` is ignored while target quality is active (it is used only as the first probe seed). Solved CRFs are cached in `tq.json` so a resume does not re-probe.

---

## `[avxs]`

avxs pipeline controls. All flags default to `false` / disabled.

| Key | Type | Default | Description |
|---|---|---|---|
| `video` | `"encode"` \| `"copy"` | `"encode"` | `copy` passes the source video through untouched and only runs the audio and subtitle steps. The video-only options below (`hdr`, `crop`, `keyint`, `scale`, `bit_depth`) and `[encoder_params]` are ignored, and `encoder` is not needed. |
| `hdr` | Boolean | `false` | Detect HDR type and pass color metadata (`--color-primaries`, `--transfer-characteristics`, `--matrix-coefficients`, `--chroma-sample-position`, `--content-light`, `--mastering-display`) to the encoder automatically. Works for HDR10, HLG, and Dolby Vision/HDR10+ (fallback to HDR10 metadata). Independent of the encoder binary chosen. |
| `crop` | Boolean | `false` | Detect black bars via `ffmpeg cropdetect` (5 samples at 10/25/40/55/70 % of the runtime, threshold 128 for HDR/10-bit). The detected crop is applied in the Y4M pipe **before** the encoder. Result is cached in `crop.cache` inside the temp directory. |
| `keyint` | Boolean | `false` | Calculate `--keyint` from source FPS for a ~5 s keyframe distance (`round(fps × 5)`). Silently skipped if `keyint` is already set in `[encoder_params]`. |
| `scale` | Integer | - | Maximum output height in pixels. The source is scaled down proportionally using Lanczos resampling if taller than this value. If the source (after crop) is already at or below this height, no scaling is applied. Example: `1080` encodes 4K content as 1080p while leaving 720p content untouched. Aspect ratio is always preserved. |
| `bit_depth` | `8` \| `10` | - | Force the encoder input bit depth. Omitted = pass the source bit depth through (SVT-AV1 default: 8→8, 10→10). Set to `10` to convert 8-bit sources to 10-bit via FFMS2 (slightly better fidelity at ~5% size cost, see SVT-AV1 docs).|
| `keep_temp` | Boolean | `false` | Keep temporary chunks and index files after encoding. |

```toml
[avxs]
hdr       = true
crop      = true
keyint    = true
scale     = 1080
bit_depth = 10
keep_temp = false
```

---

## `[audio]`

Controls how audio tracks are carried over from the source file. This is also
the default profile: any track not matched by a more specific rule uses it.

| Key | Type | Default | Description |
|---|---|---|---|
| `mode` | `"copy"` \| `"encode"` | `"copy"` | Copy or re-encode |
| `codec` | String | - | FFmpeg codec name, e.g. `"libopus"` - required when `mode = "encode"` |
| `bitrate` | String \| table | - | Target bitrate, single value or per-layout table (see below). Required when encoding to a lossy codec |
| `options` | table | `{}` | Extra encoder options, passed as `-<key> <value>`, e.g. `{ compression_level = 12 }` |
| `language_whitelist` | String array | `[]` | Keep only tracks with these language tags (ISO 639-2). Empty = keep all |

The channel count is always taken from the source. FLAC passes the source layout
through unchanged; Opus normalizes the layout name to one its encoder accepts but
never changes the channel count.

### Bitrate per channel layout

`bitrate` is either a single string applied to every track, or a table keyed by
layout. avxs detects each track's channel count and picks the matching entry;
`default` covers anything not listed.

| Channels | Key | Channels | Key |
|---|---|---|---|
| 1 | `mono` | 5 | `5.0` |
| 2 | `stereo` | 6 | `5.1` |
| 3 | `3.0` | 7 | `6.1` |
| 4 | `quad` | 8 | `7.1` |

```toml
[audio]
mode    = "encode"
codec   = "libopus"
bitrate = { stereo = "192k", "5.1" = "320k", "7.1" = "512k", default = "192k" }
```

Lossless codecs (`flac`, `alac`, `wavpack`, `pcm_*`) ignore bitrate, so it may be
omitted for them.

### Language whitelist

When `language_whitelist` is set, only audio tracks whose language tag is in the list are kept. Tracks **without a language tag** are always kept.

```toml
[audio]
language_whitelist = ["deu", "ger"]  # German only
mode = "copy"
```

Common ISO 639-2 codes: `deu`/`ger` (German), `eng` (English), `fra`/`fre` (French), `jpn` (Japanese), `und` (undefined).

---

## `[audio.lossless]`

Override applied to tracks whose **source** is lossless. Any field left unset
inherits from `[audio]`, so you usually only set `codec` and maybe `options`.

Lossless is detected automatically from ffmpeg's own codec table (`ffmpeg
-codecs`), so every codec ffmpeg flags as lossless is covered without a hardcoded
list. `dts` is special-cased: it counts as lossless only in its Master Audio
profile (`DTS-HD MA`), since ffprobe reports lossy and lossless DTS under the
same codec name.

```toml
[audio]
mode    = "encode"
codec   = "libopus"
bitrate = { stereo = "192k", "5.1" = "320k", "7.1" = "512k", default = "192k" }

[audio.lossless]
codec   = "flac"
options = { compression_level = 12 }
```

Result: lossless sources become FLAC at maximum compression, everything else
becomes Opus.

### Track titles and pre-encode summary

Re-encoded tracks keep their source name and get the new codec appended in
parentheses, e.g. `Deutsch Dolby Digital Plus 7.1` becomes `Deutsch Dolby
Digital Plus 7.1 (Opus)`. Untitled tracks get the codec name alone; copied
tracks keep their name unchanged.

Before encoding, avxs logs one line per kept audio track showing what was
detected and how it will be handled, alongside the video encoder summary:

```
audio track 0: deu eac3 5.1 (lossy) -> Opus 320k
audio track 1: ger truehd 7.1 (lossless) -> FLAC
```

---

## `[audio.codec_rules]`

Per source codec override, keyed by the codec name as reported by `ffprobe`
(lowercase). A matching rule has the highest precedence and, like
`[audio.lossless]`, inherits any unset field from `[audio]`.

| Key in rule | Type | Description |
|---|---|---|
| `mode` | `"copy"` \| `"encode"` | Inherits from `[audio]` if unset |
| `codec` | String | Inherits from `[audio]` if unset |
| `bitrate` | String \| table | Inherits from `[audio]` if unset |
| `options` | table | Inherits from `[audio]` if unset |

```toml
[audio]
language_whitelist = ["deu", "ger"]
mode = "copy"   # default: copy all codecs not matched by a rule

[audio.codec_rules]
eac3   = { mode = "encode", codec = "libopus", bitrate = "192k" }
opus   = { mode = "copy" }   # don't re-encode existing Opus
ac3    = { mode = "encode", codec = "libopus", bitrate = "128k" }
```

**Resolution order for each kept track:**
1. Filter by language whitelist (empty list = no filter)
2. Settings resolve as `codec_rules[codec]` → `[audio.lossless]` (lossless sources only) → `[audio]`. Whichever matches first wins; unset fields inherit from `[audio]`.
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
