//! APDU value type: the three frame formats I / S / U.

use crate::frame104::seq::SeqNo;

/// Maximum size of the ASDU portion of an APDU, in octets. Derived from the
/// 1-octet length field (max 253) minus the 4 control octets.
pub const MAX_ASDU_LEN: usize = 249;

/// Maximum overall APDU length on the wire (start + length + 4 control + ASDU).
pub const MAX_APDU_LEN: usize = 255;

/// U-format function (act/con of STARTDT, STOPDT, TESTFR).
///
/// Encoded as the upper 6 bits of the first control octet. Exactly one
/// of `_act` / `_con` is set per pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UFunction {
    StartDtAct,
    StartDtCon,
    StopDtAct,
    StopDtCon,
    TestFrAct,
    TestFrCon,
}

impl UFunction {
    /// Bit pattern of the first control octet for this function (with the
    /// low two bits `11` already set).
    pub const fn first_octet(self) -> u8 {
        match self {
            Self::StartDtAct => 0b0000_0111, // 0x07
            Self::StartDtCon => 0b0000_1011, // 0x0B
            Self::StopDtAct => 0b0001_0011,  // 0x13
            Self::StopDtCon => 0b0010_0011,  // 0x23
            Self::TestFrAct => 0b0100_0011,  // 0x43
            Self::TestFrCon => 0b1000_0011,  // 0x83
        }
    }

    /// Parse the first control octet into a `UFunction`, or `None` if the
    /// pattern doesn't match exactly one of the standard six.
    pub const fn from_first_octet(b: u8) -> Option<Self> {
        match b {
            0x07 => Some(Self::StartDtAct),
            0x0B => Some(Self::StartDtCon),
            0x13 => Some(Self::StopDtAct),
            0x23 => Some(Self::StopDtCon),
            0x43 => Some(Self::TestFrAct),
            0x83 => Some(Self::TestFrCon),
            _ => None,
        }
    }
}

/// Parsed APDU — one of three frame formats.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Apdu {
    /// Information transfer: an ASDU with send and receive sequence numbers.
    I {
        send: SeqNo,
        recv: SeqNo,
        asdu: Vec<u8>,
    },
    /// Supervisory: acknowledges I-frames up to (but not including) `recv`.
    S { recv: SeqNo },
    /// Unnumbered: connection-control frame.
    U { function: UFunction },
}

/// Convenience builder pattern that mirrors how protocols usually want to
/// construct an APDU — without spelling out the enum variant each time.
#[derive(Debug, Default, Clone)]
pub struct ApduPayload;

impl ApduPayload {
    pub fn info(send: SeqNo, recv: SeqNo, asdu: Vec<u8>) -> Apdu {
        Apdu::I { send, recv, asdu }
    }
    pub fn supervisory(recv: SeqNo) -> Apdu {
        Apdu::S { recv }
    }
    pub fn unnumbered(function: UFunction) -> Apdu {
        Apdu::U { function }
    }
}
