#!/usr/bin/env bash
# Spec 07 — File-transfer round-trip via the FT ASDU set (IEC 60870-5-5).
#
# Server hosts a file at NOF 0xBB3D (=47933, CRC-16/IBM of "123456789").
# Client fetches it; we verify the bytes match and the wire trace contains
# the full F_SC / F_FR / F_SR / F_SG / F_LS / F_AF sequence.

source "$(dirname "$0")/lib.sh"
setup_test "07_file_transfer"

# Stage the fixture before starting the server so the FsFileTransferProvider
# index includes it on first scan.
fixture="$SERVER_FILES/123456789"
printf 'hello from spec 07\n' > "$fixture"  # 19 bytes
expected_bytes=$(wc -c < "$fixture" | tr -d ' ')

RUST_LOG=iec60870=trace start_server
RUST_LOG=iec60870=trace start_client

# Fetch via the standard CLI subcommand.
resp=$(cli file get --nof 47933)
assert_ok "$resp" "file get"
assert_jq "$resp" '.bytes' "$expected_bytes" "byte count"
path=$(echo "$resp" | jq -r '.path')
[[ -f "$path" ]] || fail "file did not land at $path"

# Byte-identical content check.
diff -q "$fixture" "$path" > /dev/null || fail "fetched bytes differ from fixture"

# Wire-level: confirm the FT ASDU sequence on the client side.
log_clean="$WORKDIR/client.clean.log"
sed 's/\x1b\[[0-9;]*m//g' "$CLOG" > "$log_clean"

for tid in 120 121 122 123 124 125; do
    grep -qE "asdu: \[$tid," "$log_clean" \
        || fail "TypeID $tid not seen in FT exchange (RUST_LOG too low?)"
done

pass
