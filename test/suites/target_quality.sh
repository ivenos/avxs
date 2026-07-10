#!/bin/sh
# Tests for target_quality: per-chunk CVVDP JOD-targeted CRF via the bundled FFVship tool.
. "$(dirname "$0")/../lib.sh"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

# -- CVVDP target: av1 output, display model + chosen crf logged, tq.json cached -
I="$WORKDIR/1/in"; O="$WORKDIR/1/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
[target_quality]
jod        = 9.5
min_crf    = 20
max_crf    = 50
max_probes = 3
[avxs]
keep_temp = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 180 || fail "target quality: no output"
assert_video_codec  "$O/test.mkv" av1
assert_log_contains "target quality:"
assert_log_contains "target crf"
assert_file_exists  "$O/.avxs_test/tq.json"

# -- target quality together with downscale: output height honored -------------
I="$WORKDIR/2/in"; O="$WORKDIR/2/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_720p.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
[target_quality]
jod        = 9.0
min_crf    = 20
max_crf    = 50
max_probes = 2
[avxs]
scale = 480
EOF
run_avxs "$I" "$O" "$O/test.mkv" 180 || fail "target quality + scale: no output"
assert_video_codec  "$O/test.mkv" av1
assert_video_height "$O/test.mkv" 480
assert_log_contains "target quality:"

test_done
