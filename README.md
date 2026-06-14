# avxs

![Docker Image Size](https://img.shields.io/docker/image-size/ivenos/avxs)
![Docker Pulls](https://img.shields.io/docker/pulls/ivenos/avxs)

avxs is an AV1 encoding service written in Rust, distributed as a Docker image and a self-contained Linux AppImage. Drop videos and an `encode.toml` profile into a folder - avxs picks them up, splits each file into scenes, encodes the chunks in parallel with SVT-AV1, and merges everything into a finished MKV.

## Features

- **Scene-based encoding** - splits files into scenes via [av-scenechange](https://github.com/rust-av/av-scenechange), encodes all chunks in parallel, and resumes from the last completed chunk if interrupted
- **HDR passthrough** - auto-detects HDR10 and HLG, passes color metadata to the encoder automatically
- **Auto-crop** - detects and removes black bars
- **Auto-scale** - downscales to a target height
- **Auto-keyint** - derives `--keyint` from source FPS for a ~5 s keyframe interval
- **Audio control** - copy or re-encode per codec, language whitelist, per-codec rules
- **Subtitle control** - copy or strip, language whitelist

## Quick Start

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

### AppImage (Linux, x86_64 and aarch64)

Grab the latest AppImage for your architecture from the [releases page](https://github.com/ivenos/avxs/releases/latest), or run:

```sh
ARCH=$(uname -m)
wget "https://github.com/ivenos/avxs/releases/latest/download/avxs-${ARCH}.AppImage"
chmod +x "avxs-${ARCH}.AppImage"
"./avxs-${ARCH}.AppImage"
```

avxs creates `./input/` and `./output/` next to its working directory and watches them. All required tools (ffmpeg, mkvmerge, SvtAv1EncApp, ffmsindex, libffms2) are bundled inside the AppImage - nothing else to install.

---

Place an `encode.toml` next to your video files and configure your encoding profile. See [docs.md](docs.md) for the full reference.

Encoded files land flat in the output directory. Source files are moved to `input/processed/` after a successful encode.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `AVXS_INPUT_DIR` | `./input` | Input directory |
| `AVXS_OUTPUT_DIR` | `./output` | Output directory |
| `AVXS_POLL_INTERVAL` | `60` | Directory scan interval in seconds |
| `RUST_LOG` | `info` | Log verbosity. Set to `debug` for verbose output |

The official Docker image presets `AVXS_INPUT_DIR=/input` and `AVXS_OUTPUT_DIR=/output`, which is why the Compose example above mounts to those paths.

For AppImage, prefix the command: `RUST_LOG=debug ./avxs-x86_64.AppImage`.

## Supported Encoders

| `encoder` value | Binary | Version |
|---|---|---|
| `svt-av1` | `SvtAv1EncApp` | [v4.1.0](https://gitlab.com/AOMediaCodec/SVT-AV1) |
| `svt-av1-hdr` | `SvtAv1EncApp-hdr` | [cfb4e17](https://github.com/juliobbv-p/svt-av1-hdr/commit/cfb4e17693ae16945a7fe288d45437243d96c12e) (main) |

## License

[BSL 1.1](LICENSE) - free for personal and non-commercial use.
