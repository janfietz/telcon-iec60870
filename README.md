# iec60870

A Rust implementation of the IEC 60870-5 telecontrol protocols used in SCADA
and electrical-grid systems.

## Status

**0.1.0-dev** — IEC 60870-5-**104** (TCP/IP) is complete and exercised by
end-to-end tests. IEC 60870-5-101 (serial) is on the roadmap; the ASDU layer
is shared between the two so most of the heavy lifting is already done.

| Feature | State |
| --- | --- |
| ASDU envelope, VSQ, COT, CA, IOA | ✅ |
| Information elements (SIQ, DIQ, QDS, NVA, SVA, R32, BCR, CP24/56Time2a) | ✅ |
| Monitor types (M_SP_NA_1, M_DP_NA_1, M_ME_NA/NB/NC_1, time-tagged variants) | ✅ |
| Control types (C_SC/DC/RC/SE_*_*, C_IC_NA_1, time-tagged variants) | ✅ |
| System types (M_EI, C_CI, C_RD, C_CS, C_RP) | ✅ |
| IEC 60870-5-104 APCI/APDU codec | ✅ |
| 15-bit `SeqNo` with wrapping arithmetic | ✅ |
| Connection state machine (STARTDT, STOPDT, TESTFR, t0..t3, k/w window) | ✅ |
| Async client and server over `tokio` | ✅ |
| TLS via `tokio-rustls` (feature `tls`) | ✅ |
| `EventHandler` trait with `DefaultLoggingHandler` | ✅ |
| End-to-end loopback integration test | ✅ |
| IEC 60870-5-101 (serial transport, FT 1.2 framing) | 🚧 planned |
| File-transfer ASDUs (120-127) | 🚧 planned |

## Workspace

| Crate | Description |
| --- | --- |
| [`iec60870-proto`](crates/iec60870-proto) | Sans-I/O protocol layer: ASDU codec, APCI framing for IEC 60870-5-104, and the connection state machine. No async, no sockets, no clocks — every entry point takes the current `Instant` explicitly. |
| [`iec60870`](crates/iec60870) | Async client and server built on `tokio`. Plain TCP, TLS via `tokio-rustls` (feature `tls`), serial via `tokio-serial` (feature `serial`, planned). Hookable `EventHandler`. |

## Quickstart

Bring up an outstation on the standard port:

```bash
cargo run --example server_104
```

In another terminal, connect a client and watch the interrogation cycle:

```bash
RUST_LOG=iec60870=info cargo run --example client_104
```

You should see something like:

```
state changed state=Starting
state changed state=Active
[??] type_id=100 cot=Cot { cause: Cause(7), ... }   # ActivationCon
[SP] ioa=100 on=true quality=...                    # M_SP_NA_1
[ME] ioa=200 value=50 quality=...                   # M_ME_NC_1
[ME] ioa=201 value=51.5 quality=...
[??] type_id=100 cot=Cot { cause: Cause(10), ... }  # ActivationTermination
```

## Using the library

A minimal client that connects, sends a general interrogation, and prints
responses:

```rust,no_run
use bytes::BytesMut;
use iec60870::proto::asdu::cot::{Cause, Cot};
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::types::{C_IC_NA_1, Qoi};
use iec60870::proto::asdu::Asdu;
use iec60870::proto::frame104::Config;
use iec60870::{Client104, ClientEvent, Transport};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = Client104::connect(
        Transport::tcp("127.0.0.1:2404".parse()?),
        Config::default(),
    ).await?;

    let interrogation = C_IC_NA_1 { objects: vec![(Ioa(0), Qoi::GENERAL)] };
    let mut buf = BytesMut::new();
    Asdu::from_payload(
        Cot::with(Cause::ACTIVATION),
        CommonAddress(1),
        Vsq::single(1),
        &interrogation,
        AsduAddressing::IEC104,
    ).encode(&mut buf, AsduAddressing::IEC104);
    client.send_asdu(buf.to_vec()).await?;

    while let Some(ClientEvent::Asdu(bytes)) = client.recv().await {
        println!("received {} bytes", bytes.len());
    }
    Ok(())
}
```

### Custom event handlers

Implement [`EventHandler`](crates/iec60870/src/handler.rs) to hook into the
connection lifecycle:

```rust,ignore
use iec60870::EventHandler;
use iec60870::proto::frame104::{Apdu, State};

struct MetricsHandler { /* … */ }

impl EventHandler for MetricsHandler {
    fn on_frame_received(&self, _apdu: &Apdu) { /* increment counter */ }
    fn on_state_changed(&self, state: State) { /* update gauge */ }
}

let client = Client104::connect_with(transport, Config::default(), MetricsHandler { ... }).await?;
```

The library ships [`DefaultLoggingHandler`](crates/iec60870/src/handler.rs)
that routes everything to the `tracing` crate at appropriate levels.

### TLS

```rust,ignore
use std::sync::Arc;
use iec60870::{tls_client_connect, NoopHandler, Transport};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

let mut roots = RootCertStore::empty();
// roots.add(...)  -- load your CA chain

let cfg = Arc::new(
    ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth()
);

let client = tls_client_connect(
    Transport::Tls {
        addr: "scada.example:19998".parse()?,
        server_name: "scada.example".into(),
        client_config: cfg,
    },
    Config::default(),
    NoopHandler,
).await?;
```

The conventional TLS port for IEC 60870-5-104 (per IEC 62351-3) is **19998**;
constants for both that and the plaintext default (2404) are exported as
[`DEFAULT_PORT`](crates/iec60870/src/transport.rs) and `DEFAULT_TLS_PORT`.

## Design

The codebase follows the **sans-I/O** pattern popularised by `quinn-proto`
and `h2`: the protocol logic (codecs and the connection state machine) lives
in `iec60870-proto` and has zero dependency on `tokio`, sockets, or wall-clock
time. Every state-machine entry point accepts the current `Instant`
explicitly. The async crate, `iec60870`, drives that state machine over a
real transport.

Why this matters:

* The state machine is 100% deterministic — unit tests exercise k/w window
  saturation, t1 timer expiry, out-of-sequence frame detection, and the
  STARTDT/STOPDT/TESTFR handshake without any timing flakiness.
* The same state machine drives plain TCP, TLS, and (in future) serial
  transport without recompilation.
* The protocol crate can be reused for `no_std` / embedded contexts with
  minimal modification.

See [docs/protocol-notes.md](docs/protocol-notes.md) for byte-level reference
material on FT 1.2 framing, APCI/APDU layout, control-field bit maps, and
the t0..t3 timer set.

## Testing

```bash
cargo test --workspace --all-features
```

85 tests pass (83 unit + 2 integration). Property-based tests under
`proptest` cover the codec layer for round-trip integrity across every input
domain.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
