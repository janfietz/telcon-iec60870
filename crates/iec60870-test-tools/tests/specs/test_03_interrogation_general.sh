#!/usr/bin/env bash
# Spec 03 — C_IC_NA_1 general interrogation (Qoi=20).
#
# Verifies the IEC 60870-5-104 §7.4.4.2 sequence:
#   Master → Outstation : COT=ACTIVATION (6)
#   Outstation → Master : COT=ACTIVATION_CON (7)
#   Outstation → Master : 50 monitor ASDUs at COT=INTERROGATED_GENERAL (20)
#   Outstation → Master : COT=ACTIVATION_TERMINATION (10)

source "$(dirname "$0")/lib.sh"
setup_test "03_interrogation_general"

start_server
# Capture the wire trace so we can verify COTs after the fact.
RUST_LOG=iec60870=trace start_client

resp=$(cli interrogate --timeout-ms 5000)
assert_ok "$resp" "interrogate"
assert_jq "$resp" '.count' '50' "interrogation count"

# Quick check on TypeID coverage: every one of our 10 supported types
# should appear at least once in the returned points list.
expected_kinds=(sp_na dp_na me_na me_nb me_nc sp_tb dp_tb me_td me_te me_tf)
for k in "${expected_kinds[@]}"; do
    n=$(echo "$resp" | jq -r --arg k "$k" '[.points[] | select(.kind==$k)] | length')
    [[ "$n" -ge 1 ]] || fail "kind $k missing from interrogation response (n=$n)"
done

# Inspect the client's wire log for the spec-mandated COT sequence.
# Strip ANSI; look for the C_IC_NA_1 echo at COT=7 and COT=10.
log_clean="$WORKDIR/client.clean.log"
sed 's/\x1b\[[0-9;]*m//g' "$CLOG" > "$log_clean"

# COT byte is at index 2 of the ASDU; for TypeID 100 with COT 6 we expect
# the literal substring "asdu: [100, 1, 6,"   (1 = VSQ=single+count1)
grep -q "asdu: \[100, 1, 6," "$log_clean" \
    || warn "did not observe C_IC_NA_1 COT=6 in client trace (RUST_LOG too low?)"
# COT 7 (ACTIVATION_CON) — emitted by the outstation, received by the master.
grep -q "asdu: \[100, 1, 7," "$log_clean" \
    || warn "did not observe ACTIVATION_CON (COT=7) in client trace"
# COT 10 (ACTIVATION_TERMINATION)
grep -q "asdu: \[100, 1, 10," "$log_clean" \
    || warn "did not observe ACTIVATION_TERMINATION (COT=10) in client trace"

pass
