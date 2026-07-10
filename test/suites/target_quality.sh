#!/bin/sh
# Tests for target_quality. CVVDP runs on the GPU via FFVship and target_quality
# requires one; a headless CI runner has no GPU, so we verify the clear error path
# here. The actual CVVDP search (probe -> measure -> solve) is covered by the Rust
# unit tests and exercised manually on a GPU.
. "$(dirname "$0")/../lib.sh"

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

# -- No GPU: target_quality fails with a clear error, no output, no crash --------
I="$WORKDIR/1/in"; O="$WORKDIR/1/out"; mkdir -p "$I/p" "$O"
cp "$FIXTURES_DIR/sdr_simple.mkv" "$I/p/test.mkv"
cat > "$I/p/encode.toml" << 'EOF'
encoder = "svt-av1"
[encoder_params]
preset = 12
[target_quality]
jod = 9.5
EOF
run_avxs_timed "$I" "$O" 90 "requires a GPU"
assert_log_contains "requires a GPU"
assert_file_not_exists "$O/test.mkv"

test_done
