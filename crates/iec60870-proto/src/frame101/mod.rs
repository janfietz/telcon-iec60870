//! IEC 60870-5-101 — FT 1.2 framing and link-layer state machine.
//!
//! Per IEC 60870-5-1 / -2, three frame formats are defined:
//!
//! * **Single-character** — a raw `0xE5` (ACK) or `0xA2` (NACK) octet.
//! * **Fixed-length** — 5 octets (plus extra for a 2-octet link address):
//!   `0x10 C LA CS 0x16`.
//! * **Variable-length** — variable-length payload plus the doubled-length
//!   header: `0x68 L L 0x68 C LA ASDU.. CS 0x16`.
//!
//! The checksum `CS` is the **arithmetic sum mod 256** of the control byte,
//! link-address bytes, and ASDU bytes. It is not CRC-16.
//!
//! ## Modules
//!
//! * [`frame`] — the [`Frame101`] enum, [`SingleChar`], [`LinkAddress`], and
//!   [`LinkAddressSize`].
//! * [`codec`] — the stateless [`Codec`] that encodes and decodes frames.

pub mod codec;
pub mod frame;

pub use codec::Codec;
pub use frame::{Frame101, LinkAddress, LinkAddressSize, SingleChar};
