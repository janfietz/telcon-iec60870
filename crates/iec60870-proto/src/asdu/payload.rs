//! The `AsduPayload` trait: each typed ASDU payload (e.g. `M_SP_NA_1`)
//! associates its on-wire layout with a Type ID and exposes
//! [`encode_information_objects`] / [`decode_information_objects`].
//!
//! Built-in payload types live under [`crate::asdu::types`]. To add a custom
//! ASDU type, define a struct that implements [`AsduPayload`] and pass it
//! to [`crate::asdu::Asdu::with_payload`].
//!
//! [`encode_information_objects`]: AsduPayload::encode_information_objects
//! [`decode_information_objects`]: AsduPayload::decode_information_objects

use bytes::{Buf, BufMut};

use crate::asdu::header::{AsduAddressing, Vsq};
use crate::error::Result;

/// Trait implemented by every typed ASDU payload.
///
/// Implementations are responsible for serialising and deserialising **only
/// the information-objects section** of an ASDU — the surrounding header
/// (Type ID, VSQ, COT, CA) is handled generically by [`crate::asdu::Asdu`].
pub trait AsduPayload: Sized {
    /// The Type ID this payload corresponds to. Used for dispatch and
    /// asserted by [`crate::asdu::Asdu::decode_payload`].
    const TYPE_ID: u8;

    /// Encode the information-objects section. The caller has already
    /// written the Type ID, VSQ, COT and CA fields. `addressing` controls
    /// the IOA width.
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        vsq: Vsq,
        addressing: AsduAddressing,
    );

    /// Decode the information-objects section. `vsq` describes how many
    /// objects to consume and whether they share a single IOA.
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self>;
}
