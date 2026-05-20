//! FT 1.2 frame types for IEC 60870-5-101.
//!
//! Three frame formats are defined in IEC 60870-5-1:
//!
//! ```text
//! Single-char:  [0xE5]  or  [0xA2]
//!
//! Fixed-length (5 + extra LA octets):
//!   +------+----+----+----+------+
//!   | 0x10 | C  | LA | CS | 0x16 |
//!   +------+----+----+----+------+
//!
//! Variable-length:
//!   +------+----+----+------+----+----+--------+----+------+
//!   | 0x68 | L  | L  | 0x68 | C  | LA | ASDU.. | CS | 0x16 |
//!   +------+----+----+------+----+----+--------+----+------+
//! ```
//!
//! `LA` occupies 1 or 2 octets depending on the system-wide [`LinkAddressSize`]
//! configuration. The checksum `CS` is the arithmetic sum (mod 256) of the
//! control byte, all link-address bytes, and all ASDU bytes.

// ---------------------------------------------------------------------------
// SingleChar
// ---------------------------------------------------------------------------

/// A single-character frame — a raw acknowledgement octet.
///
/// ```text
/// | Octet | Value | Meaning       |
/// |-------|-------|---------------|
/// |   0   | 0xE5  | Positive ACK  |
/// |   0   | 0xA2  | Negative ACK  |
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SingleChar {
    /// Positive acknowledgement (`0xE5`).
    Ack = 0xE5,
    /// Negative acknowledgement (`0xA2`).
    Nack = 0xA2,
}

impl SingleChar {
    /// Parse a raw byte into a `SingleChar`, returning `None` if the byte is
    /// not a valid single-character frame value.
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0xE5 => Some(Self::Ack),
            0xA2 => Some(Self::Nack),
            _ => None,
        }
    }

    /// The wire byte for this single-character frame.
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

// ---------------------------------------------------------------------------
// LinkAddress
// ---------------------------------------------------------------------------

/// Width of the link address field (1 or 2 octets).
///
/// This is a system-wide configuration parameter negotiated at deployment
/// time. It must be the same on both ends of the link.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkAddressSize {
    /// 1-octet link address (values 0..=255).
    #[default]
    One,
    /// 2-octet link address (values 0..=65535, little-endian on the wire).
    Two,
}

impl LinkAddressSize {
    /// Number of wire octets consumed by this address size.
    pub const fn len(self) -> usize {
        match self {
            Self::One => 1,
            Self::Two => 2,
        }
    }

    /// Returns `false` — a link address always occupies at least one octet.
    ///
    /// Required alongside [`Self::len`] to satisfy `clippy::len_without_is_empty`.
    pub const fn is_empty(self) -> bool {
        false
    }
}

/// A link-layer address (1 or 2 octets on the wire, always stored as `u16`).
///
/// The wire encoding is little-endian for the 2-octet variant, matching the
/// general IEC 60870-5 convention for multi-octet integers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LinkAddress(pub u16);

impl LinkAddress {
    /// Encode this address into a byte slice using `size`-wide encoding.
    /// Returns the number of bytes written (1 or 2).
    pub fn encode_to(self, buf: &mut impl bytes::BufMut, size: LinkAddressSize) {
        match size {
            LinkAddressSize::One => buf.put_u8(self.0 as u8),
            LinkAddressSize::Two => buf.put_u16_le(self.0),
        }
    }

    /// Decode a link address from a byte slice.
    ///
    /// Returns `None` if the slice is too short for the requested size.
    pub fn decode_from(buf: &[u8], size: LinkAddressSize) -> Option<Self> {
        match size {
            LinkAddressSize::One => buf.first().map(|&b| Self(b as u16)),
            LinkAddressSize::Two => {
                if buf.len() < 2 {
                    None
                } else {
                    Some(Self(u16::from_le_bytes([buf[0], buf[1]])))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Frame101
// ---------------------------------------------------------------------------

/// An IEC 60870-5-101 FT 1.2 frame.
///
/// Three formats are defined by the standard:
///
/// * [`Single`](Frame101::Single) — a raw 1-octet ACK/NACK.
/// * [`Fixed`](Frame101::Fixed) — 5-octet control frame (no payload).
/// * [`Variable`](Frame101::Variable) — variable-length frame carrying an ASDU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame101 {
    /// Single-character frame (0xE5 = ACK, 0xA2 = NACK).
    Single(SingleChar),
    /// Fixed-length control frame (5 octets for 1-byte LA; 6 for 2-byte LA).
    ///
    /// Wire layout:
    /// ```text
    /// +------+----+------+----+------+
    /// | 0x10 | C  |  LA  | CS | 0x16 |
    /// +------+----+------+----+------+
    /// ```
    Fixed {
        /// Control field byte.
        control: u8,
        /// Link address.
        address: LinkAddress,
    },
    /// Variable-length frame carrying an ASDU.
    ///
    /// Wire layout:
    /// ```text
    /// +------+----+----+------+----+------+--------+----+------+
    /// | 0x68 | L  | L  | 0x68 | C  |  LA  | ASDU.. | CS | 0x16 |
    /// +------+----+----+------+----+------+--------+----+------+
    /// ```
    /// where `L = 1 (control) + LA_size + len(ASDU)`.
    Variable {
        /// Control field byte.
        control: u8,
        /// Link address.
        address: LinkAddress,
        /// Raw ASDU bytes (application payload).
        asdu: Vec<u8>,
    },
}
