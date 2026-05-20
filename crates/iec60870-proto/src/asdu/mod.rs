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
//! See [`docs/protocol-notes.md`](../../../../docs/protocol-notes.md) for the
//! full byte-level reference.

pub mod ie;
