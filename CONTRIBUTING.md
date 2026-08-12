# Contributing

## Setup

Rust 1.95 or newer - the MSRV floor in `Cargo.toml`; the Docker and AppImage
builds pin a newer toolchain of their own. `build.rs` links `libffms2`
dynamically, so a local build needs its headers plus the assembler the
scene-detection crate builds with:

```bash
sudo apt-get install -y libffms2-dev nasm
cargo build
```

Running the binary outside a container also needs `ffmpeg`, `ffprobe`,
`mkvmerge`, `ffmsindex`, `FFVship` and `SvtAv1EncApp` on `PATH`. Building the
image is the shortcut, because it carries all of them:

```bash
docker build -t avxs:test .
```

## Tests

Unit tests are `#[cfg(test)]` modules next to the code they cover:

```bash
cargo test --locked
```

Integration tests run the real image end to end. `test/run.sh` builds the
image, generates the fixture videos with ffmpeg inside a container, then runs
every suite in `test/suites/`:

```bash
./test/run.sh                 # build + fixtures + all suites
./test/run.sh --no-build      # reuse the existing image
./test/run.sh --verbose       # container logs on failure
./test/run.sh audio           # only suites whose name matches "audio"
```

They test the binary inside the image, not the working tree, so a stale image
means a green run that proves nothing. The `Dockerfile` touches the sources
before `cargo build` for exactly that reason; leave it in.

They encode real files, so budget minutes rather than seconds. Each suite sets
`AVXS_POLL_INTERVAL` high enough that the container scans once and then idles,
so a suite that hangs is a suite whose expected output never appeared.

"Expected 0 tracks" is the assertion of every negative case, and a missing file
answers it just as convincingly as a correct one: an assertion that cannot read
what it came to check has to fail, `run_avxs` requires a non-empty output file,
and a run that executes no test case exits non-zero.

For running the image by hand against your own samples, `test/local/` is
gitignored and yours to fill:

```yaml
# test/local/compose.yml
services:
  avxs:
    build: ../..
    image: avxs:local
    volumes:
      - ./input:/input:z
      - ./output:/output:z
    environment:
      AVXS_POLL_INTERVAL: "10"
```

Put a profile in `test/local/input/<profile>/encode.toml` and your samples
beside it. Not in `input/processed/`, which the scanner skips.

CI runs the unit tests, then builds the image with the layer cache and runs the
integration suite against it, and only then publishes anything.

## Code style

- Errors are `anyhow::Result` with a `.context()` naming the step that failed.
  A failed job is logged and skipped and must never take the scan loop down
  with it; release builds set `panic = "abort"`, so a panic ends the process.
- A failure that clears on its own or after an edit the user was going to make
  anyway - a source still being copied in, a profile with a typo - carries
  `job::Transient` in its context chain and is retried on the next scan.
  Everything else writes a `.failed` marker that locks the file out until a
  human deletes it, so add the marker only where a retry really is pointless.
- Every external process avxs only waits on gets a timeout:
  `ext::output_with_timeout` in async code, `ext::blocking_output_with_timeout`
  on the chunk workers. Nothing supervises this process, so a hung tool would
  stop the queue for good. The limit bounds a wedge, not the runtime.
  Exempt are the three children avxs feeds through a pipe - the two encoders and
  the scene-detection ffmpeg - which the pipe paces already. Drain their stderr
  on a thread; they write a progress line per frame and will block on a full pipe.
- A probe that walks the whole file rather than reading a header (`-show_packets`,
  `-count_packets`, `-show_frames` past the first interval) needs
  `ext::ffprobe_json_with_timeout` and a limit of its own.
- Every ffmpeg call that reads the source names its stream: `-map 0:v:0`, or
  `-map 0:a:<n>` per track. Left to itself ffmpeg picks by resolution, while
  FFMS2 takes the first video track and every ffprobe here asks for `v:0`.
- Anything that lands in the output directory is written to a scratch name and
  renamed into place once it validates; the scanner reads "output exists" as
  "already done". State files go the same way, flushed before the rename, and
  every reader treats zero length as absent - the fingerprint will not clear one.
- A comment only earns its place when it records something the code cannot: an
  ffmpeg quirk, an SVT-AV1 parameter constraint, the reason for a non-obvious
  filter chain or a metric threshold. One or two lines. Never restate what the
  code already says.
- Rust 2024. There is no formatter or clippy gate in CI, so match the file you
  are editing, including where it aligns values into columns.

## External tools

avxs shells out to its encoders and muxers, resolving each through
`ext::external_bin`: a binary next to the avxs executable first, `PATH` after.
That order is what makes the AppImage self-contained.

Currently called: `ffmpeg`, `ffprobe`, `mkvmerge`, `ffmsindex`, `FFVship`, and
`SvtAv1EncApp` / `SvtAv1EncApp-hdr`.

Adding one is a three-part change: the call site, the runtime stage of the
`Dockerfile`, and the AppDir staging step in `.github/workflows/appimage.yml`.
Miss either of the last two and the code still compiles, but one of the shipped
artefacts fails at encode time.

## Configuration keys

Every knob is a key in a profile's `encode.toml`, parsed in `src/config.rs`.
A new key is a four-part change: the field with its default, a check in
`Config::validate` if the value can be wrong, a test covering both an accepted
and a rejected value, and its row or section in the README. A key that is not
in the README does not exist.

Every config struct carries `#[serde(deny_unknown_fields)]`; keep it that way on
new ones, or a typo in a section name reads as "this feature was not configured"
and the job runs on silently.

Validation is worth the line it costs: `encode.toml` is written by hand, and a
value that only fails after ffprobe, scene detection and half an encode fails at
the worst possible moment.

Defaults live in the `Default` impls and are repeated in the README tables, so
changing one means changing both.

## Commits

Conventional Commits (https://www.conventionalcommits.org/en/v1.0.0/), with a
short imperative subject.

## Releases

`Cargo.toml` stays at `version = "0.0.0"`; the real version is the git tag.

A push to `main` builds the multi-arch image and publishes it as `dev`. A `v*`
tag publishes the semver tags and moves `latest`. Publishing a GitHub release
also triggers the AppImage workflow, which attaches the x86_64 and aarch64
AppImages to it. The changelog lives in the release notes, in Keep a Changelog
style (https://keepachangelog.com/en/1.1.0/); there is no CHANGELOG.md.

## Dependencies

`Cargo.lock` is committed and CI builds `--locked`, so a dependency change has
to bring the updated lock file with it.

The pinned external sources - SVT-AV1, SVT-AV1-HDR, FFMS2, Vship and the Rust
toolchain - exist twice, as `ARG`s in the `Dockerfile` and as `env` in
`.github/workflows/appimage.yml`. Renovate groups both into one PR as long as
the two agree, so keep them in lockstep: let them drift and the image and the
AppImage ship different encoders. SVT-AV1-HDR is a rolling repo and is pinned to
a commit rather than a tag.

Base images and GitHub Actions stay on version tags, never commit SHAs or
digests. linuxdeploy is pinned to a release tag rather than its rolling
`continuous` build, so the same avxs tag keeps producing the same AppImage.
Renovate opens the bumps; keep them out of behaviour changes.

## Pull requests

- One concern per PR. Add tests for behaviour changes.
- `cargo test` and `./test/run.sh` must pass.
- Fill in the PR template, including the test plan (which fixture or sample,
  which profile, unit or integration) and the CLA checkbox.
