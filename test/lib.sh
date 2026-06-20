#!/bin/sh
# Shared test library. Source this file from each test case.
#
# Provides:
#   run_avxs        INPUT OUTPUT EXPECTED_FILE [TIMEOUT_S]
#   run_avxs_timed  INPUT OUTPUT WAIT_S [LOG_PATTERN]
#   assert_*        various assertion helpers
#   test_done       call at end of each test case to exit with correct code
#
# Environment:
#   AVXS_IMAGE      Docker image to use (default: avxs:test)
#   FIXTURES_DIR    Path to test fixtures (default: sibling fixtures/ dir)
#   VERBOSE=1       Print Docker logs on failure

AVXS_IMAGE="${AVXS_IMAGE:-avxs:test}"
FIXTURES_DIR="${FIXTURES_DIR:-$(cd "$(dirname "$0")/../fixtures" 2>/dev/null && pwd)}"

_FAIL=0
_ERRORS=""
AVXS_LOGS=""
_ESC=$(printf '\033')

# -- Failure tracking --------------------------------------------------------

fail() {
    _FAIL=1
    _ERRORS="${_ERRORS}  > $1
"
}

# -- Docker helpers ----------------------------------------------------------

# Run avxs and wait until EXPECTED_FILE appears (or TIMEOUT_S elapses).
# Sets AVXS_LOGS with container stdout+stderr.
# Returns 0 if the file appeared, 1 if timed out.
run_avxs() {
    local input="$1" output="$2" expected="$3" timeout="${4:-120}"
    AVXS_LOGS=""

    local cid
    cid=$(docker run -d \
        --user "$(id -u):$(id -g)" \
        -v "${input}:/input:z" \
        -v "${output}:/output:z" \
        -e AVXS_POLL_INTERVAL=999999 \
        -e "RUST_LOG=${AVXS_RUST_LOG:-info}" \
        "${AVXS_IMAGE}")

    local elapsed=0
    while [ "$elapsed" -lt "$timeout" ]; do
        [ -e "$expected" ] && break
        local running
        running=$(docker inspect -f '{{.State.Running}}' "$cid" 2>/dev/null) || running="false"
        [ "$running" = "false" ] && break
        sleep 1
        elapsed=$((elapsed + 1))
    done

    # Output file appears at mux time but source is only moved to processed/
    # several lines later. Wait for "[stem] done" to confirm full cleanup.
    if [ -e "$expected" ]; then
        local stem done_wait=0
        stem=$(basename "$expected" .mkv)
        while [ "$done_wait" -lt 10 ]; do
            docker logs "$cid" 2>&1 | sed "s/${_ESC}\[[0-9;]*m//g" | \
                grep -qF "[$stem] done" && break
            sleep 1
            done_wait=$((done_wait + 1))
        done
    fi

    AVXS_LOGS=$(docker logs "$cid" 2>&1) || true
    docker rm -f "$cid" >/dev/null 2>&1 || true

    [ -e "$expected" ] && return 0 || return 1
}

# Run avxs for up to WAIT seconds, then stop. With an optional LOG_PATTERN it
# returns as soon as that pattern appears in the logs (capped at WAIT); without
# one it waits the full WAIT. Useful for negative tests where no output is expected.
# Always returns 0; sets AVXS_LOGS.
run_avxs_timed() {
    local input="$1" output="$2" wait="${3:-15}" pattern="${4:-}"
    AVXS_LOGS=""

    local cid
    cid=$(docker run -d \
        --user "$(id -u):$(id -g)" \
        -v "${input}:/input:z" \
        -v "${output}:/output:z" \
        -e AVXS_POLL_INTERVAL=999999 \
        -e "RUST_LOG=${AVXS_RUST_LOG:-info}" \
        "${AVXS_IMAGE}")

    if [ -n "$pattern" ]; then
        local elapsed=0
        while [ "$elapsed" -lt "$wait" ]; do
            docker logs "$cid" 2>&1 | sed "s/${_ESC}\[[0-9;]*m//g" | \
                grep -qF "$pattern" && break
            sleep 1
            elapsed=$((elapsed + 1))
        done
    else
        sleep "$wait"
    fi

    AVXS_LOGS=$(docker logs "$cid" 2>&1) || true
    docker rm -f "$cid" >/dev/null 2>&1 || true
    return 0
}

# -- Assertions ---------------------------------------------------------------

assert_file_exists() {
    [ -f "$1" ] || fail "expected file to exist: $1"
}

assert_file_nonempty() {
    [ -s "$1" ] || fail "expected non-empty file: $1"
}

assert_file_not_exists() {
    [ ! -f "$1" ] || fail "expected file NOT to exist: $1"
}

assert_dir_exists() {
    [ -d "$1" ] || fail "expected directory to exist: $1"
}

assert_dir_not_exists() {
    [ ! -d "$1" ] || fail "expected directory NOT to exist: $1"
}

assert_audio_track_count() {
    local file="$1" expected="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams a \
        -show_entries stream=codec_type -of csv=p=0 "$file" 2>/dev/null | \
        grep "audio" | wc -l | tr -d ' ')
    [ "$actual" = "$expected" ] || \
        fail "audio track count: expected $expected, got $actual ($file)"
}

assert_audio_codec() {
    local file="$1" idx="$2" expected="$3"
    local actual
    actual=$(ffprobe -v quiet -select_streams "a:${idx}" \
        -show_entries stream=codec_name -of csv=p=0 "$file" 2>/dev/null | \
        tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "audio track $idx codec: expected $expected, got $actual ($file)"
}

assert_subtitle_track_count() {
    local file="$1" expected="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams s \
        -show_entries stream=codec_type -of csv=p=0 "$file" 2>/dev/null | \
        grep "subtitle" | wc -l | tr -d ' ')
    [ "$actual" = "$expected" ] || \
        fail "subtitle track count: expected $expected, got $actual ($file)"
}

assert_video_height() {
    local file="$1" expected="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=height -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "video height: expected $expected, got $actual ($file)"
}

assert_video_height_le() {
    local file="$1" max="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=height -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "${actual:-0}" -le "$max" ] || \
        fail "video height: expected <= $max, got $actual ($file)"
}

assert_video_codec() {
    local file="$1" expected="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=codec_name -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "video codec: expected $expected, got $actual ($file)"
}

assert_audio_channels() {
    local file="$1" idx="$2" expected="$3"
    local actual
    actual=$(ffprobe -v quiet -select_streams "a:${idx}" \
        -show_entries stream=channels -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "audio track $idx channels: expected $expected, got $actual ($file)"
}

assert_audio_title() {
    local file="$1" idx="$2" expected="$3"
    local actual
    actual=$(ffprobe -v quiet -select_streams "a:${idx}" \
        -show_entries stream_tags=title -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "audio track $idx title: expected '$expected', got '$actual' ($file)"
}

assert_color_transfer() {
    local file="$1" expected="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=color_transfer -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "color_transfer: expected $expected, got $actual ($file)"
}

assert_color_primaries() {
    local file="$1" expected="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=color_primaries -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "color_primaries: expected $expected, got $actual ($file)"
}

assert_video_pix_fmt() {
    local file="$1" expected="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=pix_fmt -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "$actual" = "$expected" ] || \
        fail "pix_fmt: expected $expected, got $actual ($file)"
}

assert_video_height_lt() {
    local file="$1" max="$2"
    local actual
    actual=$(ffprobe -v quiet -select_streams v:0 \
        -show_entries stream=height -of csv=p=0 "$file" 2>/dev/null | tr -d '\n')
    [ "${actual:-0}" -lt "$max" ] || \
        fail "video height: expected < $max, got $actual ($file)"
}

assert_log_contains() {
    printf '%s\n' "$AVXS_LOGS" | sed "s/${_ESC}\[[0-9;]*m//g" | grep -qF "$1" || \
        fail "log does not contain: $1"
}

assert_log_not_contains() {
    printf '%s\n' "$AVXS_LOGS" | sed "s/${_ESC}\[[0-9;]*m//g" | grep -qF "$1" && \
        fail "log should NOT contain: $1" || true
}

# -- Test lifecycle ------------------------------------------------------------

# Call at the end of every test case. Prints errors and exits with correct code.
test_done() {
    if [ "$_FAIL" -eq 0 ]; then
        exit 0
    fi
    printf "%s" "$_ERRORS"
    if [ "${VERBOSE:-0}" = "1" ] && [ -n "$AVXS_LOGS" ]; then
        printf "  [Docker logs]\n"
        echo "$AVXS_LOGS" | while IFS= read -r line; do
            printf "  | %s\n" "$line"
        done
    fi
    exit 1
}
