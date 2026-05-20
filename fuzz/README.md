# IEC 60870 Fuzz Targets

Libfuzzer-based harnesses for the three parser entry points.

## Prerequisites

```bash
rustup install nightly
cargo install cargo-fuzz
```

## Running

All commands must be run from this directory (`fuzz/`).

```bash
cd fuzz

# Fuzz the IEC 60870-5-104 APDU decoder
cargo +nightly fuzz run apdu_decode

# Fuzz the ASDU envelope decoder (IEC 60870-5-104 addressing profile)
cargo +nightly fuzz run asdu_decode

# Fuzz the IEC 60870-5-101 FT 1.2 frame decoder (both 1- and 2-byte address widths)
cargo +nightly fuzz run frame101_decode
```

Use `-- -max_total_time=60` to run each target for a fixed duration:

```bash
cargo +nightly fuzz run apdu_decode -- -max_total_time=60
```

Corpus is saved under `fuzz/corpus/<target>/`. Minimise a crashing input with:

```bash
cargo +nightly fuzz tmin apdu_decode artifacts/apdu_decode/<crash-file>
```

## Targets

| Target | Parser | Goal |
| --- | --- | --- |
| `apdu_decode` | `frame104::Codec::decode_slice` | No panic on any byte sequence |
| `asdu_decode` | `asdu::Asdu::decode` | No panic on any byte sequence |
| `frame101_decode` | `frame101::Codec::decode_slice` | No panic on any byte sequence (both address widths) |

The fuzz crate is excluded from the workspace (`exclude = ["fuzz"]` in the
root `Cargo.toml`) so that `cargo test --workspace` does not require a
nightly toolchain.
