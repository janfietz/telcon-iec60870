//! Application Service Data Unit codec, shared between IEC 60870-5-101 and -104.
//!
//! Layout (per IEC 60870-5-101 §7 / IEC 60870-5-104 §7):
//!
//! ```text
//! +--------+-----+-----+----+--------------+
//! | TypeID | VSQ | COT | CA | InfoObjects  |
//! +--------+-----+-----+----+--------------+
//! ```
//!
//! [`Asdu`] is the wire-level envelope: it carries the header fields and the
//! information-objects section as raw bytes. To interpret the bytes as a
//! specific Type ID, call [`Asdu::decode_payload`] with one of the types
//! defined in [`types`] (or your own type implementing [`AsduPayload`]).
//!
//! See [`docs/protocol-notes.md`](../../../../docs/protocol-notes.md) for the
//! full byte-level reference.

pub mod cot;
pub mod envelope;
pub mod header;
pub mod ie;
pub mod payload;
pub mod types;

pub use cot::{Cause, Cot};
pub use envelope::Asdu;
pub use header::{AsduAddressing, CaSize, CommonAddress, CotSize, Ioa, IoaSize, Vsq};
pub use payload::AsduPayload;
