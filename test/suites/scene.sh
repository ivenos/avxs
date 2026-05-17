#!/bin/sh
# Tests for scene.rs: chunk splitting, speed, downscale, scenes.json reuse.
. "$(dirname "$0")/../lib.sh"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

# -- extra_split_sec=5 on 60s source: produces more chunks than default --------
I="$WORKDIR/1/in"; O="$WORKDIR/1/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_long.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keep_temp = true
[scene_detection]
extra_split_sec = 5
EOF
run_avxs "$I" "$O" "$O/test.mkv" 300 || fail "extra_split: no output"
CHUNK_COUNT=$(grep -c '"index"' "$O/.avxs_test/scenes.json" 2>/dev/null || echo 0)
[ "$CHUNK_COUNT" -gt 6 ] || fail "extra_split: expected >6 chunks, got $CHUNK_COUNT"

# -- speed=fast: encode completes without error --------------------------------
I="$WORKDIR/2/in"; O="$WORKDIR/2/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[scene_detection]
speed = "fast"
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "fast speed: no output"
assert_file_nonempty "$O/test.mkv"

# -- downscale_height: affects detection only, output resolution unchanged -----
I="$WORKDIR/3/in"; O="$WORKDIR/3/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_720p.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[scene_detection]
downscale_height = 180
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "downscale: no output"
assert_video_height "$O/test.mkv" 720

# -- pre-created scenes.json is reused, detection skipped ---------------------
I="$WORKDIR/4/in"; O="$WORKDIR/4/out"; mkdir -p "$I/p" "$O/.avxs_test"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
printf '[{"index":0,"start_frame":0,"end_frame":239}]\n' > "$O/.avxs_test/scenes.json"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keep_temp = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "scenes reuse: no output"
assert_log_contains "reusing scenes.json"

# -- extra_split in frames overrides extra_split_sec when set -----------------
I="$WORKDIR/5/in"; O="$WORKDIR/5/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_long.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keep_temp = true
[scene_detection]
extra_split     = 120
extra_split_sec = 999
EOF
run_avxs "$I" "$O" "$O/test.mkv" 300 || fail "extra_split frames: no output"
CHUNK_COUNT=$(grep -c '"index"' "$O/.avxs_test/scenes.json" 2>/dev/null || echo 0)
[ "$CHUNK_COUNT" -gt 6 ] || fail "extra_split frames: expected >6 chunks, got $CHUNK_COUNT"

test_done
