#!/bin/sh
# Tests for encode.rs: output codec, encoder param injection, keyint and HDR override logic.
. "$(dirname "$0")/../lib.sh"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

# -- output video codec is av1 -------------------------------------------------
I="$WORKDIR/1/in"; O="$WORKDIR/1/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "codec: no output"
assert_video_codec "$O/test.mkv" av1

# -- manual keyint in encoder_params: auto-keyint logged but not injected ------
I="$WORKDIR/2/in"; O="$WORKDIR/2/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
keyint = 240
[avxs]
keyint = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "keyint override: no output"
assert_log_contains     "auto-keyint"
assert_log_contains     "keyint=240"

# -- auto-HDR param skipped when encoder_params has the same key ---------------
I="$WORKDIR/3/in"; O="$WORKDIR/3/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/hdr10.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset           = 12
crf              = 50
color-primaries  = 1
[avxs]
hdr = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "HDR override: no output"
assert_log_contains     "color-primaries=1"
assert_color_primaries  "$O/test.mkv" "bt709"

# -- auto-keyint with no manual override: keyint appears in encoder args -------
I="$WORKDIR/4/in"; O="$WORKDIR/4/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keyint = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "auto-keyint: no output"
assert_log_contains "auto-keyint"
assert_log_contains "keyint="

# -- SDR source + hdr=true: "HDR: SDR" logged, encode succeeds ----------------
I="$WORKDIR/5/in"; O="$WORKDIR/5/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
hdr = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "SDR+hdr: no output"
assert_log_contains    "HDR: SDR"
assert_file_nonempty   "$O/test.mkv"

test_done
