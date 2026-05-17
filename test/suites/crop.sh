#!/bin/sh
# Tests for crop.rs: cropdetect, cache, crop+scale interaction.
. "$(dirname "$0")/../lib.sh"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

# -- letterboxed source: height reduced after crop -----------------------------
I="$WORKDIR/1/in"; O="$WORKDIR/1/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_blackbars.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
crop = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "crop: no output"
assert_video_height_lt "$O/test.mkv" 480
assert_log_contains    "auto-crop"

# -- clean source: cropdetect finds no bars, height unchanged -----------------
I="$WORKDIR/2/in"; O="$WORKDIR/2/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
crop = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "no-crop: no output"
assert_video_height  "$O/test.mkv" 360
assert_log_contains  "no black bars"

# -- crop cache hit: second run uses cached result -----------------------------
I="$WORKDIR/3/in"; O="$WORKDIR/3/out"; mkdir -p "$I/p" "$O/.avxs_test"
cp "$FIXTURES_DIR/sdr_blackbars.mkv" "$I/p/test.mkv"
printf 'crop=640:360:0:60' > "$O/.avxs_test/crop.cache"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
crop      = true
keep_temp = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "crop cache: no output"
assert_log_contains "(cached)"

# -- crop + scale: result is <=240 (cropdetect round=16 may give 352, not 360) -
I="$WORKDIR/4/in"; O="$WORKDIR/4/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_blackbars.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
crop  = true
scale = 240
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "crop+scale: no output"
assert_video_height_le "$O/test.mkv" 240

# -- fresh detection writes crop.cache ----------------------------------------
I="$WORKDIR/5/in"; O="$WORKDIR/5/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_blackbars.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
crop      = true
keep_temp = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "cache persist: no output"
assert_file_exists "$O/.avxs_test/crop.cache"
[ -s "$O/.avxs_test/crop.cache" ] || fail "crop.cache is empty after detection"

# -- empty cache → "no black bars (cached)" -----------------------------------
I="$WORKDIR/6/in"; O="$WORKDIR/6/out"; mkdir -p "$I/p" "$O/.avxs_test"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
printf '' > "$O/.avxs_test/crop.cache"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
crop = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "empty cache: no output"
assert_log_contains "no black bars (cached)"

test_done
