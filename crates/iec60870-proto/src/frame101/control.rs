//! Control field for the FT 1.2 link layer.
//!
//! The control field is a single octet with the following bit layout (MSB → LSB):
//!
//! ```text
//! | 7   | 6   | 5       | 4       | 3..0     |
//! | RES | PRM | FCB/ACD | FCV/DFC | FuncCode |
//! ```
//!
//! * **PRM** (bit 6) — Primary message. Set to 1 in frames sent by the primary
//!   (master) station, 0 in frames sent by the secondary (outstation).
//! * **FCB** (bit 5, primary) — Frame Count Bit. Alternates on each
//!   SEND/CONFIRM cycle so the outstation can detect duplicates.
//! * **ACD** (bit 5, secondary) — Access Demand. Set by the outstation when it
//!   has class-1 data ready.
//! * **FCV** (bit 4, primary) — Frame Count Valid. When 0, FCB is ignored by
//!   the outstation (used for unconfirmed sends and requests).
//! * **DFC** (bit 4, secondary) — Data Flow Control. Set by the outstation
//!   when its receive buffer is full.
//! * **FuncCode** (bits 3..0) — Function code; meaning depends on `PRM`.

// ---------------------------------------------------------------------------
// Direction
// ---------------------------------------------------------------------------

/// Which side of the link is sending the control field.
///
/// The meaning of bits 5 and 4 in the control byte differs between primary
/// and secondary directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Primary station (master) sending; bits 5/4 = FCB/FCV.
    Primary,
    /// Secondary station (outstation) sending; bits 5/4 = ACD/DFC.
    Secondary,
}

// ---------------------------------------------------------------------------
// Function codes
// ---------------------------------------------------------------------------

/// Primary-direction function codes (bits 3..0 when PRM=1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FuncCodePrimary {
    /// Reset of remote link (`0`).
    ResetRemoteLink = 0,
    /// Reset of user process (`1`).
    ResetUserProcess = 1,
    /// User data, confirmed — SEND/CONFIRM (`3`).
    UserDataConfirmed = 3,
    /// User data, unconfirmed — SEND/NO REPLY (`4`).
    UserDataUnconfirmed = 4,
    /// Request status of link (`9`).
    RequestStatus = 9,
    /// Request user data class 1 — REQ/RESP (`10`).
    RequestUserDataClass1 = 10,
    /// Request user data class 2 — REQ/RESP (`11`).
    RequestUserDataClass2 = 11,
}

impl FuncCodePrimary {
    /// Parse a 4-bit function code (bits 3..0 of the control byte).
    pub const fn from_nibble(nibble: u8) -> Option<Self> {
        match nibble & 0x0F {
            0 => Some(Self::ResetRemoteLink),
            1 => Some(Self::ResetUserProcess),
            3 => Some(Self::UserDataConfirmed),
            4 => Some(Self::UserDataUnconfirmed),
            9 => Some(Self::RequestStatus),
            10 => Some(Self::RequestUserDataClass1),
            11 => Some(Self::RequestUserDataClass2),
            _ => None,
        }
    }
}

/// Secondary-direction function codes (bits 3..0 when PRM=0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FuncCodeSecondary {
    /// Acknowledge (`0`).
    Ack = 0,
    /// Negative acknowledge (`1`).
    Nack = 1,
    /// Response with user data (`8`).
    RespondUserData = 8,
    /// NACK — no user data available (`9`).
    NackNoData = 9,
    /// Status of link or access demand (`11`).
    StatusOfLink = 11,
}

impl FuncCodeSecondary {
    /// Parse a 4-bit function code (bits 3..0 of the control byte).
    pub const fn from_nibble(nibble: u8) -> Option<Self> {
        match nibble & 0x0F {
            0 => Some(Self::Ack),
            1 => Some(Self::Nack),
            8 => Some(Self::RespondUserData),
            9 => Some(Self::NackNoData),
            11 => Some(Self::StatusOfLink),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// ControlField
// ---------------------------------------------------------------------------

/// Decoded control field for an FT 1.2 frame.
///
/// Two variants cover the primary (master) and secondary (outstation)
/// directions, because bits 5 and 4 have different meanings in each:
///
/// | Bit | Primary  | Secondary |
/// |-----|----------|-----------|
/// |  6  | PRM=1    | PRM=0     |
/// |  5  | FCB      | ACD       |
/// |  4  | FCV      | DFC       |
///
/// # Wire encoding
///
/// ```text
/// | 7   | 6   | 5       | 4       | 3..0     |
/// | RES | PRM | FCB/ACD | FCV/DFC | FuncCode |
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlField {
    /// Frame sent by the primary (master) station.
    Primary {
        /// Frame Count Bit — alternates on each SEND/CONFIRM cycle.
        fcb: bool,
        /// Frame Count Valid — when `false`, `fcb` is ignored.
        fcv: bool,
        /// Function code.
        func: FuncCodePrimary,
    },
    /// Frame sent by the secondary (outstation) station.
    Secondary {
        /// Access Demand — outstation has class-1 data ready.
        acd: bool,
        /// Data Flow Control — outstation receive buffer full.
        dfc: bool,
        /// Function code.
        func: FuncCodeSecondary,
    },
}

/// Bit masks for the control field octet.
const PRM_BIT: u8 = 0x40; // bit 6
const FCB_ACD_BIT: u8 = 0x20; // bit 5
const FCV_DFC_BIT: u8 = 0x10; // bit 4
const FUNC_MASK: u8 = 0x0F; // bits 3..0

impl ControlField {
    /// Encode this control field as a single wire byte.
    ///
    /// Bit 7 (reserved) is always zero.
    pub fn encode(self) -> u8 {
        match self {
            Self::Primary { fcb, fcv, func } => {
                let mut b = PRM_BIT | (func as u8 & FUNC_MASK);
                if fcb {
                    b |= FCB_ACD_BIT;
                }
                if fcv {
                    b |= FCV_DFC_BIT;
                }
                b
            }
            Self::Secondary { acd, dfc, func } => {
                // PRM bit is 0 for secondary
                let mut b = func as u8 & FUNC_MASK;
                if acd {
                    b |= FCB_ACD_BIT;
                }
                if dfc {
                    b |= FCV_DFC_BIT;
                }
                b
            }
        }
    }

    /// Decode a wire byte using the supplied `direction` to disambiguate
    /// bits 5 and 4.
    ///
    /// Returns `None` if the function-code nibble is not a recognised value
    /// for the given direction.
    pub fn decode(byte: u8, direction: Direction) -> Option<Self> {
        let fcb_acd = byte & FCB_ACD_BIT != 0;
        let fcv_dfc = byte & FCV_DFC_BIT != 0;
        let nibble = byte & FUNC_MASK;
        match direction {
            Direction::Primary => {
                let func = FuncCodePrimary::from_nibble(nibble)?;
                Some(Self::Primary {
                    fcb: fcb_acd,
                    fcv: fcv_dfc,
                    func,
                })
            }
            Direction::Secondary => {
                let func = FuncCodeSecondary::from_nibble(nibble)?;
                Some(Self::Secondary {
                    acd: fcb_acd,
                    dfc: fcv_dfc,
                    func,
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ---- encoding individual function codes --------------------------------

    #[test]
    fn primary_reset_remote_link_encodes() {
        let cf = ControlField::Primary {
            fcb: false,
            fcv: false,
            func: FuncCodePrimary::ResetRemoteLink,
        };
        // PRM(0x40) + func(0x00) = 0x40
        assert_eq!(cf.encode(), 0x40);
    }

    #[test]
    fn primary_user_data_confirmed_with_fcb_fcv() {
        let cf = ControlField::Primary {
            fcb: true,
            fcv: true,
            func: FuncCodePrimary::UserDataConfirmed,
        };
        // PRM(0x40) | FCB(0x20) | FCV(0x10) | func(0x03) = 0x73
        assert_eq!(cf.encode(), 0x73);
    }

    #[test]
    fn secondary_ack_encodes() {
        let cf = ControlField::Secondary {
            acd: false,
            dfc: false,
            func: FuncCodeSecondary::Ack,
        };
        // no PRM, no ACD, no DFC, func=0 → 0x00
        assert_eq!(cf.encode(), 0x00);
    }

    #[test]
    fn secondary_respond_user_data_with_acd() {
        let cf = ControlField::Secondary {
            acd: true,
            dfc: false,
            func: FuncCodeSecondary::RespondUserData,
        };
        // ACD(0x20) | func(0x08) = 0x28
        assert_eq!(cf.encode(), 0x28);
    }

    // ---- decode / roundtrip ------------------------------------------------

    #[test]
    fn decode_primary_request_status() {
        // PRM=1 | func=9 → 0x49
        let cf = ControlField::decode(0x49, Direction::Primary).unwrap();
        assert_eq!(
            cf,
            ControlField::Primary {
                fcb: false,
                fcv: false,
                func: FuncCodePrimary::RequestStatus,
            }
        );
    }

    #[test]
    fn decode_secondary_status_of_link() {
        // func=11 = 0x0B
        let cf = ControlField::decode(0x0B, Direction::Secondary).unwrap();
        assert_eq!(
            cf,
            ControlField::Secondary {
                acd: false,
                dfc: false,
                func: FuncCodeSecondary::StatusOfLink,
            }
        );
    }

    #[test]
    fn fcb_toggle_bit_pattern() {
        // Two consecutive confirmed sends — FCB should alternate.
        let first = ControlField::Primary {
            fcb: false,
            fcv: true,
            func: FuncCodePrimary::UserDataConfirmed,
        };
        let second = ControlField::Primary {
            fcb: true,
            fcv: true,
            func: FuncCodePrimary::UserDataConfirmed,
        };
        let b1 = first.encode();
        let b2 = second.encode();
        // FCB bit must differ
        assert_ne!(b1 & FCB_ACD_BIT, b2 & FCB_ACD_BIT);
    }

    #[test]
    fn unknown_primary_func_returns_none() {
        // Nibble 0x02 is not defined for primary
        assert!(ControlField::decode(0x42, Direction::Primary).is_none());
    }

    #[test]
    fn unknown_secondary_func_returns_none() {
        // Nibble 0x02 is not defined for secondary
        assert!(ControlField::decode(0x02, Direction::Secondary).is_none());
    }

    // ---- proptest ----------------------------------------------------------

    proptest! {
        #[test]
        fn prop_primary_roundtrip(
            fcb: bool,
            fcv: bool,
            func_raw in prop_oneof![
                Just(FuncCodePrimary::ResetRemoteLink),
                Just(FuncCodePrimary::ResetUserProcess),
                Just(FuncCodePrimary::UserDataConfirmed),
                Just(FuncCodePrimary::UserDataUnconfirmed),
                Just(FuncCodePrimary::RequestStatus),
                Just(FuncCodePrimary::RequestUserDataClass1),
                Just(FuncCodePrimary::RequestUserDataClass2),
            ]
        ) {
            let cf = ControlField::Primary { fcb, fcv, func: func_raw };
            let byte = cf.encode();
            let decoded = ControlField::decode(byte, Direction::Primary).unwrap();
            prop_assert_eq!(decoded, cf);
        }

        #[test]
        fn prop_secondary_roundtrip(
            acd: bool,
            dfc: bool,
            func_raw in prop_oneof![
                Just(FuncCodeSecondary::Ack),
                Just(FuncCodeSecondary::Nack),
                Just(FuncCodeSecondary::RespondUserData),
                Just(FuncCodeSecondary::NackNoData),
                Just(FuncCodeSecondary::StatusOfLink),
            ]
        ) {
            let cf = ControlField::Secondary { acd, dfc, func: func_raw };
            let byte = cf.encode();
            let decoded = ControlField::decode(byte, Direction::Secondary).unwrap();
            prop_assert_eq!(decoded, cf);
        }
    }
}
