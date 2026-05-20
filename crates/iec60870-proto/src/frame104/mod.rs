//! IEC 60870-5-104 — APCI framing, sequence numbers, and connection state machine.
//!
//! Per IEC 60870-5-104 §5.1, every APDU on the wire is:
//!
//! ```text
//! +------+--------+--+--+--+--+----+
//! | 0x68 |  len   |C1|C2|C3|C4| .. |
//! +------+--------+--+--+--+--+----+
//! ```
//!
//! where `len` counts the four control octets plus the trailing ASDU. The
//! low bits of C1 select one of three formats:
//!
//! * **I** (information transfer) — carries an ASDU; numbered with N(S), N(R).
//! * **S** (supervisory) — pure acknowledgement.
//! * **U** (unnumbered) — connection control: STARTDT, STOPDT, TESTFR.

pub mod apdu;
pub mod codec;
pub mod seq;
pub mod state;

pub use apdu::{Apdu, ApduPayload, UFunction};
pub use codec::Codec;
pub use seq::SeqNo;
pub use state::{Action, Config, Connection, DisconnectReason, Input, Role, State};
