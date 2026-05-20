# iec60870

A Rust implementation of the IEC 60870-5-101 and IEC 60870-5-104 telecontrol
protocols used in SCADA / electrical-grid systems.

## Status

Pre-alpha — under active development. Not yet released to crates.io.

## Workspace

| Crate | Description |
| --- | --- |
| [`iec60870-proto`](crates/iec60870-proto) | Sans-I/O protocol layer: ASDU codec, FT 1.2 framing (101), APCI framing and connection state machine (104). No async, no sockets, deterministic. |
| [`iec60870`](crates/iec60870) | Async client and server built on `tokio`. Plain TCP, TLS via `tokio-rustls` (feature `tls`), serial via `tokio-serial`. Hookable `EventHandler`. |

## Design Goals

- **Sans-I/O core.** Protocol logic is pure code, easy to unit- and property-test.
- **TLS first-class.** Encrypted IEC 60870-5-104 via `tokio-rustls` (cf. IEC 62351-3).
- **Hooks, not callbacks.** A zero-cost `EventHandler` trait with a default `tracing`-based logging implementation.
- **High test coverage.** Property-based tests for codecs; in-memory transport tests for the async layer.

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
