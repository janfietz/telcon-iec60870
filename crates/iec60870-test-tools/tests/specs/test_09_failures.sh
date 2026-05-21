#!/usr/bin/env bash
# Spec 09 — Expected-failure cases.
#
# Each sub-case asserts that an invalid input produces a clean error response
# (`ok=false` + a non-empty `error`), not a hang, panic, or silent success.
# When the test is happy, every sub-case prints its individual outcome and
# the script ends with PASS. A single unexpected outcome aborts.

source "$(dirname "$0")/lib.sh"
setup_test "09_failures"

start_server
start_client

# ── 09.a — get on an unknown IOA ────────────────────────────────────────────
log "09.a get unknown ioa"
resp=$(srv get --ioa 99999)
assert_fail "$resp" "get unknown ioa"
err=$(echo "$resp" | jq -r '.error')
[[ -n "$err" ]] || fail "09.a: error field is empty"

# ── 09.b — set on an unknown IOA ───────────────────────────────────────────
log "09.b set on unknown ioa"
resp=$(srv set --ioa 99999 --kind sp-na --value true)
assert_fail "$resp" "set unknown ioa"
err=$(echo "$resp" | jq -r '.error')
echo "$err" | grep -qi "not found" || fail "09.b: expected 'not found' in error, got: $err"

# ── 09.b2 — set with a kind string the CLI doesn't recognize ───────────────
log "09.b2 set with unrecognised kind syntax"
if srv set --ioa 100 --kind no-such-kind --value true >/dev/null 2>&1; then
    fail "09.b2: bogus --kind should make the CLI exit non-zero"
fi

# ── 09.c — file get for an unknown NOF (≈30 s idle timeout in FT service) ──
log "09.c file get unknown nof (this case takes ~30 s — that's the FT idle timeout)"
resp=$(cli file get --nof 1 2>&1 || true)
# The client returns an `ok=false` with an "idle timeout" error.
assert_fail "$resp" "file get unknown nof"
err=$(echo "$resp" | jq -r '.error')
echo "$err" | grep -qi "timeout" || fail "09.c: expected timeout-related error, got: $err"

# ── 09.d — server-side op invoked on the client daemon ────────────────────
log "09.d server-side op on client socket"
# The client doesn't have an `iec-client set` subcommand at the CLI level,
# but the wire-level Request::Set sent to the client socket should return
# "not a client op". We use socat-style raw NDJSON.
resp=$(python3 - <<EOF
import socket, json, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("$CSOCK")
s.sendall(b'{"op":"set","ioa":100,"value":{"kind":"single","value":true}}\n')
data = b""
while b"\n" not in data:
    chunk = s.recv(4096)
    if not chunk: break
    data += chunk
sys.stdout.write(data.decode())
EOF
)
assert_fail "$resp" "set on client"
err=$(echo "$resp" | jq -r '.error')
echo "$err" | grep -qi "not a client op" || fail "09.d: expected 'not a client op', got: $err"

# ── 09.e — second daemon on the same TCP port ─────────────────────────────
log "09.e second daemon on busy port"
# Try to start a second server bound to the same port; it should die within
# a few hundred ms with an "Address already in use" error in its log.
SECOND_LOG="$WORKDIR/server2.log"
SECOND_SOCK="$WORKDIR/server2.sock"
"$SERVER_BIN" --control "$SECOND_SOCK" daemon \
    --transport tcp --addr "127.0.0.1:$PORT" \
    > "$SECOND_LOG" 2>&1 &
SECOND_PID=$!
sleep 0.6
if kill -0 "$SECOND_PID" 2>/dev/null; then
    kill -KILL "$SECOND_PID" 2>/dev/null || true
    fail "09.e: second server stayed alive on busy port"
fi
grep -qiE "address already in use|addrinuse" "$SECOND_LOG" \
    || fail "09.e: second server died but log doesn't say 'address in use': $(cat "$SECOND_LOG")"

# ── 09.f — malformed JSON over the control socket ─────────────────────────
log "09.f malformed JSON"
resp=$(python3 - <<EOF
import socket, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect("$SSOCK")
s.sendall(b"this is not json\n")
data = b""
while b"\n" not in data:
    chunk = s.recv(4096)
    if not chunk: break
    data += chunk
sys.stdout.write(data.decode())
EOF
)
assert_fail "$resp" "malformed JSON"
err=$(echo "$resp" | jq -r '.error')
echo "$err" | grep -qi "parse" || fail "09.f: expected parse error, got: $err"

pass
