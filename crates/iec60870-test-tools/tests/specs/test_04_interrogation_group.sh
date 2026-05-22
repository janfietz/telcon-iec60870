#!/usr/bin/env bash
# Spec 04 — C_IC_NA_1 group interrogation (Qoi 21..30 = groups 1..10).
#
# Each interrogation group is mapped to one of the 10 TypeID ranges by the
# outstation. The simulator concurrently emits spontaneous M_*_T*_1 ASDUs,
# so the master's interrogation collector receives both interrogated and
# spontaneous data in one buffer — we assert on the *subset matching the
# expected kind*, not the raw count.

source "$(dirname "$0")/lib.sh"
setup_test "04_interrogation_group"

start_server
start_client

# (group → expected kind for the mapped IOA range)
declare -a MAP=(
    "1 sp_na"
    "2 dp_na"
    "3 me_na"
    "4 me_nb"
    "5 me_nc"
    "6 sp_tb"
    "7 dp_tb"
    "8 me_td"
    "9 me_te"
    "10 me_tf"
)

for entry in "${MAP[@]}"; do
    # shellcheck disable=SC2206
    parts=($entry)
    g="${parts[0]}"; expected_kind="${parts[1]}"

    resp=$(cli interrogate --group "$g" --timeout-ms 2000)
    assert_ok "$resp" "group $g interrogate"

    # At least 5 points of the mapped kind should be present (the 5 IOAs of
    # that type, possibly plus repeats from simulator overlap).
    n=$(echo "$resp" | jq -r --arg k "$expected_kind" '[.points[] | select(.kind==$k)] | length')
    [[ "$n" -ge 5 ]] || fail "group $g (kind=$expected_kind): expected ≥5, got $n"

    # IOAs of the mapped range should all be present.
    base=$(( g <= 5 ? g * 100 : (g - 5) * 100 + 1000 ))
    for off in 0 1 2 3 4; do
        ioa=$(( base + off ))
        present=$(echo "$resp" | jq -r --argjson i "$ioa" --arg k "$expected_kind" \
            '[.points[] | select(.kind==$k and .ioa==$i)] | length')
        [[ "$present" -ge 1 ]] || fail "group $g IOA $ioa not in response"
    done
done

# Groups 11..16 are unmapped; they ACK but the type-specific count is 0.
for g in 11 12 13 14 15 16; do
    resp=$(cli interrogate --group "$g" --timeout-ms 1500)
    assert_ok "$resp" "group $g interrogate (empty)"
    # Allow spontaneous noise — but no INTERROGATED_GROUP_N points should be
    # there. We can't directly verify COT from the response (no COT field),
    # so just confirm we got an ok envelope and the daemon didn't crash.
done

pass
