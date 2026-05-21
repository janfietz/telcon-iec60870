# Shared helpers for reproducible E2E test procedures.
#
# Sourced by every test_*.sh. Sets up unique paths, manages daemon lifecycles,
# provides JSON assertions. After source, callers should invoke:
#
#   setup_test "name"          # creates a clean workdir + names sockets/ports
#   start_server [extra-args]  # starts iec-server daemon, exports SERVER_PID
#   start_client [extra-args]  # starts iec-client daemon, exports CLIENT_PID
#   srv <subcmd> [args]        # call iec-server CLI against the daemon
#   cli <subcmd> [args]        # call iec-client CLI against the daemon
#   assert_ok <json> [msg]     # assert .ok == true on a JSON response
#   assert_fail <json> [msg]   # assert .ok == false (expected failure)
#   assert_eq <actual> <expected> [msg]
#   assert_jq <json> <jq-expr> <expected> [msg]
#   teardown                   # kills daemons, removes temp files
#
# Tests must call `pass` on success or `fail "reason"` on a failure that
# wasn't already caught by an assertion. `set -euo pipefail` and a trap on
# EXIT ensure cleanup runs even on assertion failure.

set -euo pipefail

# Path to the worktree root (two directories up from tests/specs/).
SPECS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKTREE_ROOT="$(cd "$SPECS_DIR/../../../.." && pwd)"
SERVER_BIN="${IEC_SERVER_BIN:-$WORKTREE_ROOT/target/debug/iec-server}"
CLIENT_BIN="${IEC_CLIENT_BIN:-$WORKTREE_ROOT/target/debug/iec-client}"

if [[ ! -x "$SERVER_BIN" ]] || [[ ! -x "$CLIENT_BIN" ]]; then
    echo "FAIL: binaries not built at $SERVER_BIN / $CLIENT_BIN"
    echo "  run: cargo build -p iec60870-test-tools --bins"
    exit 2
fi

# --- pretty output -----------------------------------------------------------

c_reset=$'\e[0m'
c_red=$'\e[31m'
c_green=$'\e[32m'
c_yellow=$'\e[33m'
c_dim=$'\e[2m'

log()    { printf "%s[%s] %s%s\n" "$c_dim" "$(date +%H:%M:%S.%3N)" "$*" "$c_reset" >&2; }
pass()   { printf "%sPASS%s %s\n" "$c_green" "$c_reset" "${TEST_NAME:-(unknown)}"; exit 0; }
fail()   { printf "%sFAIL%s %s — %s\n" "$c_red" "$c_reset" "${TEST_NAME:-(unknown)}" "$*"; exit 1; }
warn()   { printf "%swarn%s %s\n" "$c_yellow" "$c_reset" "$*" >&2; }

# --- setup / teardown --------------------------------------------------------

setup_test() {
    TEST_NAME="$1"
    NONCE=$(date +%s%N | tail -c 8)
    PID=$$
    WORKDIR=$(mktemp -d "/tmp/iec-spec-${TEST_NAME}-${PID}-XXXXXX")
    SSOCK="$WORKDIR/server.sock"
    CSOCK="$WORKDIR/client.sock"
    SLOG="$WORKDIR/server.log"
    CLOG="$WORKDIR/client.log"
    SERVER_FILES="$WORKDIR/server-files"
    CLIENT_FILES="$WORKDIR/client-files"
    mkdir -p "$SERVER_FILES" "$CLIENT_FILES"
    PORT=$(pick_free_port)
    log "test=$TEST_NAME workdir=$WORKDIR port=$PORT"
    trap teardown EXIT
}

pick_free_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

start_server() {
    local extra=("$@")
    RUST_LOG="${RUST_LOG:-iec60870=warn,iec60870_test_tools=info}" \
        "$SERVER_BIN" --control "$SSOCK" daemon \
        --transport tcp --addr "127.0.0.1:$PORT" \
        --files-dir "$SERVER_FILES" "${extra[@]}" \
        >"$SLOG" 2>&1 &
    SERVER_PID=$!
    wait_for_socket "$SSOCK" 3 || { dump_log "$SLOG"; fail "server daemon did not bind socket"; }
}

start_client() {
    local extra=("$@")
    RUST_LOG="${RUST_LOG:-iec60870=warn,iec60870_test_tools=info}" \
        "$CLIENT_BIN" daemon \
        --transport tcp --addr "127.0.0.1:$PORT" \
        --control "$CSOCK" --files-dir "$CLIENT_FILES" "${extra[@]}" \
        >"$CLOG" 2>&1 &
    CLIENT_PID=$!
    wait_for_socket "$CSOCK" 3 || { dump_log "$CLOG"; fail "client daemon did not bind socket"; }
    # Give STARTDT_CON a moment to propagate to the cache.
    sleep 0.4
}

wait_for_socket() {
    local sock="$1" deadline=$(( $(date +%s) + ${2:-3} ))
    while (( $(date +%s) < deadline )); do
        [[ -S "$sock" ]] && return 0
        sleep 0.05
    done
    return 1
}

dump_log() {
    echo "--- $1 ---" >&2
    tail -40 "$1" >&2 || true
}

teardown() {
    local rc=$?
    # Try graceful shutdown via control sockets, then SIGTERM, then SIGKILL.
    if [[ -n "${CLIENT_PID:-}" ]] && kill -0 "$CLIENT_PID" 2>/dev/null; then
        "$CLIENT_BIN" shutdown --socket "$CSOCK" >/dev/null 2>&1 || true
    fi
    if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        "$SERVER_BIN" --control "$SSOCK" shutdown >/dev/null 2>&1 || true
    fi
    sleep 0.2
    for p in "${CLIENT_PID:-}" "${SERVER_PID:-}"; do
        [[ -z "$p" ]] && continue
        kill -0 "$p" 2>/dev/null && kill -TERM "$p" 2>/dev/null || true
    done
    sleep 0.2
    for p in "${CLIENT_PID:-}" "${SERVER_PID:-}"; do
        [[ -z "$p" ]] && continue
        kill -0 "$p" 2>/dev/null && kill -KILL "$p" 2>/dev/null || true
    done
    if [[ "${KEEP_WORKDIR:-0}" != "1" && -n "${WORKDIR:-}" ]]; then
        rm -rf "$WORKDIR"
    else
        [[ -n "${WORKDIR:-}" ]] && log "kept $WORKDIR"
    fi
    return "$rc"
}

# --- wrappers ---------------------------------------------------------------

srv() {
    "$SERVER_BIN" --control "$SSOCK" "$@"
}

cli() {
    local sub="$1"; shift
    case "$sub" in
        cmd|file)
            local sub2="$1"; shift
            "$CLIENT_BIN" "$sub" "$sub2" --socket "$CSOCK" "$@"
            ;;
        *)
            "$CLIENT_BIN" "$sub" --socket "$CSOCK" "$@"
            ;;
    esac
}

# --- assertions --------------------------------------------------------------

assert_eq() {
    local actual="$1" expected="$2" msg="${3:-equal}"
    if [[ "$actual" != "$expected" ]]; then
        fail "$msg: expected '$expected', got '$actual'"
    fi
}

assert_ok() {
    local json="$1" msg="${2:-ok response}"
    local ok err
    ok=$(echo "$json" | jq -r '.ok // false')
    if [[ "$ok" != "true" ]]; then
        err=$(echo "$json" | jq -r '.error // "no error msg"')
        fail "$msg: response.ok=false, error=$err"
    fi
}

assert_fail() {
    local json="$1" msg="${2:-expected failure}"
    local ok
    ok=$(echo "$json" | jq -r '.ok // false')
    if [[ "$ok" == "true" ]]; then
        fail "$msg: expected response.ok=false, but got true"
    fi
}

assert_jq() {
    local json="$1" expr="$2" expected="$3" msg="${4:-jq path}"
    local actual
    actual=$(echo "$json" | jq -r "$expr")
    if [[ "$actual" != "$expected" ]]; then
        fail "$msg: $expr → '$actual' (expected '$expected')"
    fi
}
