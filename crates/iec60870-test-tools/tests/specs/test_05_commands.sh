#!/usr/bin/env bash
# Spec 05 — Every control TypeID is accepted and produces ACTIVATION_CON
# (positive). C_SC, C_DC, C_RC, C_SE_NA/NB/NC.

source "$(dirname "$0")/lib.sh"
setup_test "05_commands"

start_server
start_client

# C_SC_NA_1 (single command)
resp=$(cli cmd single --ioa 2100 --on)
assert_ok "$resp" "single on"
assert_jq "$resp" '.cot' 'activation_con' "single on COT"
assert_jq "$resp" '.negative' 'false' "single on positive"

# `--on` is a flag in this CLI (presence = ON, absence = OFF).
resp=$(cli cmd single --ioa 2100)
assert_ok "$resp" "single off"

# C_DC_NA_1 (double command)
resp=$(cli cmd double --ioa 2200 --on)
assert_ok "$resp" "double on"
assert_jq "$resp" '.cot' 'activation_con' "double on COT"

resp=$(cli cmd double --ioa 2200)
assert_ok "$resp" "double off"

# C_RC_NA_1 (regulating-step)
resp=$(cli cmd regulating --ioa 2300 --step higher)
assert_ok "$resp" "regulating higher"
assert_jq "$resp" '.cot' 'activation_con' "regulating higher COT"

resp=$(cli cmd regulating --ioa 2300 --step lower)
assert_ok "$resp" "regulating lower"

# C_SE_NA_1 (set-point normalized)
resp=$(cli cmd setpoint --ioa 2400 --kind normalized --value=0.5)
assert_ok "$resp" "setpoint normalized"
assert_jq "$resp" '.cot' 'activation_con' "setpoint normalized COT"

# C_SE_NB_1 (set-point scaled)
resp=$(cli cmd setpoint --ioa 2400 --kind scaled --value=42)
assert_ok "$resp" "setpoint scaled"

# C_SE_NC_1 (set-point float)
resp=$(cli cmd setpoint --ioa 2500 --kind float --value=-3.14)
assert_ok "$resp" "setpoint float"

pass
