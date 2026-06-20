#!/bin/sh
# Tests for scanner.rs: profile discovery, skip logic, file extensions, env vars.
. "$(dirname "$0")/../lib.sh"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

# -- no encode.toml: profile silently skipped ----------------------------------
I="$WORKDIR/1/in"; O="$WORKDIR/1/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
run_avxs_timed "$I" "$O" 15
assert_file_not_exists "$O/test.mkv"
assert_file_exists     "$I/p/test.mkv"
assert_log_not_contains "ERROR"

# -- existing output: job skipped, source not moved ----------------------------
I="$WORKDIR/2/in"; O="$WORKDIR/2/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
printf "sentinel" > "$O/test.mkv"
SENTINEL_SIZE=$(wc -c < "$O/test.mkv")
AVXS_RUST_LOG=debug run_avxs_timed "$I" "$O" 15 "skip: output exists"
CURRENT_SIZE=$(wc -c < "$O/test.mkv" 2>/dev/null || echo 0)
[ "$CURRENT_SIZE" = "$SENTINEL_SIZE" ] || fail "existing output was overwritten"
assert_file_exists     "$I/p/test.mkv"
assert_log_contains    "skip: output exists"

# -- processed/ dir is never scanned for new jobs -----------------------------
I="$WORKDIR/3/in"; O="$WORKDIR/3/out"
mkdir -p "$I/processed" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/processed/test.mkv"
cat > "$I/processed/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
run_avxs_timed "$I" "$O" 15
assert_file_not_exists "$O/test.mkv"

# -- AVXS_POLL_INTERVAL env var is logged at startup --------------------------
I="$WORKDIR/4/in"; O="$WORKDIR/4/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
AVXS_LOGS=""
CID=$(docker run -d \
    --user "$(id -u):$(id -g)" \
    -v "${I}:/input:z" \
    -v "${O}:/output:z" \
    -e AVXS_POLL_INTERVAL=42 \
    -e RUST_LOG=info \
    "${AVXS_IMAGE:-avxs:test}")
ELAPSED=0
while [ "$ELAPSED" -lt 120 ]; do
    [ -f "$O/test.mkv" ] && break
    sleep 1; ELAPSED=$((ELAPSED + 1))
done
AVXS_LOGS=$(docker logs "$CID" 2>&1) || true
docker rm -f "$CID" >/dev/null 2>&1 || true
assert_log_contains "poll_s=42"

# -- .mp4 and .webm extensions both recognized by scanner ---------------------
I="$WORKDIR/5/in"; O="$WORKDIR/5/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/a.mp4"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/b.webm"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
crf    = 50
EOF
run_avxs "$I" "$O" "$O/b.mkv" 240 || fail "extensions: no output"
assert_file_nonempty "$O/a.mkv"
assert_file_nonempty "$O/b.mkv"
assert_file_exists   "$I/processed/a.mp4"
assert_file_exists   "$I/processed/b.webm"

# -- invalid AVXS_POLL_INTERVAL: warning logged, default used -----------------
I="$WORKDIR/6/in"; O="$WORKDIR/6/out"; mkdir -p "$I" "$O"
CID=$(docker run -d \
    --user "$(id -u):$(id -g)" \
    -v "${I}:/input:z" \
    -v "${O}:/output:z" \
    -e AVXS_POLL_INTERVAL=notanumber \
    -e RUST_LOG=warn \
    "${AVXS_IMAGE:-avxs:test}")
sleep 5
AVXS_LOGS=$(docker logs "$CID" 2>&1) || true
docker rm -f "$CID" >/dev/null 2>&1 || true
assert_log_contains "invalid value"

test_done
