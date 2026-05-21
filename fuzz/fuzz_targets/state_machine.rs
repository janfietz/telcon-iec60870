#![no_main]
//! Fuzz the IEC 60870-5-104 connection state machine.
//!
//! Drives [`Connection::handle`] with a stream of arbitrary APDUs decoded
//! from the input bytes, plus synthetic `Tick`s and user requests. The
//! fuzz input shape is "a sequence of well-formed APDUs followed by stray
//! bytes that act as control-channel inputs," chosen so libfuzzer can
//! amplify meaningful state-machine transitions (sequence-number arith,
//! window saturation, t1/t2/t3 deadlines, STARTDT/STOPDT/TESTFR handshakes)
//! without spending most of its budget rediscovering valid APDU framing.

use std::time::{Duration, Instant};

use iec60870_proto::frame104::{Codec, Config, Connection, Input, Role};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // The first byte chooses the role and decides whether the user has
    // already issued STARTDT — exercises both client- and server-side
    // entry points without inflating the fuzzed input.
    let role = if data[0] & 1 == 0 { Role::Client } else { Role::Server };
    let pre_started = data[0] & 2 != 0;
    let mut rest = &data[1..];

    let mut conn = Connection::new(role, Config::default());
    let mut t = Instant::now();

    if pre_started && role == Role::Client {
        let _ = conn.handle(Input::StartDt, t);
        t += Duration::from_millis(1);
    }

    let mut steps = 0u32;
    while !rest.is_empty() && steps < 256 {
        steps += 1;
        // Use the low 3 bits of the first byte as a sub-opcode that picks
        // between feeding bytes to the APDU codec and feeding a synthetic
        // user input. This way the fuzzer can explore both paths.
        let op = rest[0] & 0b111;
        rest = &rest[1..];

        match op {
            0..=3 => {
                // APDU-feeding path.
                match Codec::decode_slice(rest) {
                    Ok(Some((apdu, consumed))) => {
                        rest = &rest[consumed..];
                        let _ = conn.handle(Input::Apdu(apdu), t);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        // Skip a byte and try to resync, mimicking what
                        // the async driver does.
                        if !rest.is_empty() {
                            rest = &rest[1..];
                        }
                    }
                }
            }
            4 => {
                let _ = conn.handle(Input::Tick, t);
            }
            5 => {
                let _ = conn.handle(Input::StartDt, t);
            }
            6 => {
                let _ = conn.handle(Input::StopDt, t);
            }
            7 => {
                // SendAsdu — consume a length-prefixed payload from rest.
                let len = rest.first().copied().unwrap_or(0) as usize;
                if rest.len() < 1 + len {
                    break;
                }
                let payload = rest[1..1 + len].to_vec();
                rest = &rest[1 + len..];
                let _ = conn.handle(Input::SendAsdu(payload), t);
            }
            _ => unreachable!(),
        }

        // Advance the synthetic clock by a small bounded step so timers
        // can fire (t1=15s, t2=10s, t3=20s with default config).
        t += Duration::from_millis(500);
    }
});
