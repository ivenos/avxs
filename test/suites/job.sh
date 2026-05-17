#!/bin/sh
# Tests for job.rs: full encode pipeline, lifecycle, scaling, resume, failure handling.
. "$(dirname "$0")/../lib.sh"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

# -- baseline: output created, source in processed/, temp dir removed ----------
I="$WORKDIR/1/in"; O="$WORKDIR/1/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "baseline: no output"
assert_file_nonempty   "$O/test.mkv"
assert_file_exists     "$I/processed/test.mkv"
assert_file_not_exists "$I/p/test.mkv"
assert_dir_not_exists  "$O/.avxs_test"

# -- keep_temp=true: temp dir preserved ---------------------------------------
I="$WORKDIR/2/in"; O="$WORKDIR/2/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keep_temp = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "keep_temp=true: no output"
assert_dir_exists "$O/.avxs_test"

# -- keep_temp=false: temp dir removed ----------------------------------------
I="$WORKDIR/3/in"; O="$WORKDIR/3/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keep_temp = false
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "keep_temp=false: no output"
assert_dir_not_exists "$O/.avxs_test"

# -- scale down: 720p → 360p ---------------------------------------------------
I="$WORKDIR/4/in"; O="$WORKDIR/4/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_720p.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
scale = 360
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "scale down: no output"
assert_video_height  "$O/test.mkv" 360
assert_log_contains  "auto-scale"
assert_log_contains  "workers:"

# -- scale noop: source smaller than target, no scaling applied ---------------
I="$WORKDIR/5/in"; O="$WORKDIR/5/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
scale = 720
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "scale noop: no output"
assert_video_height "$O/test.mkv" 360

# -- resume: pre-created frame index is reused ---------------------------------
I="$WORKDIR/6/in"; O="$WORKDIR/6/out"; mkdir -p "$I/p" "$O/.avxs_test"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keep_temp = true
EOF
docker run --rm \
    --user "$(id -u):$(id -g)" \
    -v "${I}:/input:z" \
    -v "${O}:/output:z" \
    --entrypoint ffmsindex \
    "${AVXS_IMAGE:-avxs:test}" \
    /input/p/test.mkv /output/.avxs_test/frame-index.ffindex
[ -f "$O/.avxs_test/frame-index.ffindex" ] || fail "resume: ffmsindex produced no index"
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "resume: no output"
assert_log_contains "reusing existing index"

# -- multi-file: two videos in same profile, both encoded and moved ------------
I="$WORKDIR/7/in"; O="$WORKDIR/7/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/alpha.mkv"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/beta.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
run_avxs "$I" "$O" "$O/beta.mkv" 240 || fail "multi-file: no output"
assert_file_nonempty "$O/alpha.mkv"
assert_file_nonempty "$O/beta.mkv"
assert_file_exists   "$I/processed/alpha.mkv"
assert_file_exists   "$I/processed/beta.mkv"

# -- multiple profiles: two profile dirs, independent configs ------------------
I="$WORKDIR/8/in"; O="$WORKDIR/8/out"
mkdir -p "$I/movies" "$I/series" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv"  "$I/movies/alpha.mkv"
cp "$FIXTURES_DIR/sdr_720p.mkv"    "$I/series/beta.mkv"
cat > "$I/movies/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
cat > "$I/series/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
scale = 360
EOF
run_avxs "$I" "$O" "$O/beta.mkv" 240 || fail "multi-profile: no beta output"
assert_file_nonempty "$O/alpha.mkv"
assert_file_nonempty "$O/beta.mkv"
assert_video_height  "$O/beta.mkv" 360
assert_file_exists   "$I/processed/alpha.mkv"
assert_file_exists   "$I/processed/beta.mkv"

# -- .failed workflow: write marker, block retry, recover after fix ------------
I="$WORKDIR/9/in"; O="$WORKDIR/9/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
this-flag-doesnt-exist = "boom"
EOF
run_avxs_timed "$I" "$O" 30
assert_file_not_exists "$O/test.mkv"
assert_file_exists     "$O/.avxs_test/.failed"
assert_log_contains    "job failed"

run_avxs_timed "$I" "$O" 15
assert_log_contains "permanently failed"

rm -f "$O/.avxs_test/.failed"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail ".failed recovery: no output"
assert_file_nonempty "$O/test.mkv"
assert_file_exists   "$I/processed/test.mkv"

# -- done.json resume: second run skips already-encoded chunks ----------------
I="$WORKDIR/10/in"; O="$WORKDIR/10/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
[avxs]
keep_temp = true
EOF
run_avxs "$I" "$O" "$O/test.mkv" 120 || fail "done resume: first encode failed"
cp "$I/processed/test.mkv" "$I/p/test.mkv"
rm  "$O/test.mkv"
AVXS_RUST_LOG=debug run_avxs "$I" "$O" "$O/test.mkv" 90 || fail "done resume: second encode failed"
assert_log_contains  "already done"
assert_file_nonempty "$O/test.mkv"

test_done
