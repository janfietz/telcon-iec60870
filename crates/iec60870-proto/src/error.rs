use thiserror::Error;

/// Result type used throughout the proto crate.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors produced by codecs and state machines in this crate.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("buffer too short: needed {needed} bytes, had {have}")]
    Incomplete { needed: usize, have: usize },

    #[error("invalid start byte: expected 0x{expected:02X}, got 0x{got:02X}")]
    InvalidStartByte { expected: u8, got: u8 },

    #[error("invalid end byte: expected 0x{expected:02X}, got 0x{got:02X}")]
    InvalidEndByte { expected: u8, got: u8 },

    #[error("length mismatch: length octet says {declared} but frame body is {actual} bytes")]
    LengthMismatch { declared: usize, actual: usize },

    #[error("length octets disagree: first={first}, second={second}")]
    LengthOctetsDiffer { first: u8, second: u8 },

    #[error("checksum mismatch: expected 0x{expected:02X}, got 0x{got:02X}")]
    ChecksumMismatch { expected: u8, got: u8 },

    #[error("unknown ASDU type id: {0}")]
    UnknownAsduType(u8),

    #[error("invalid value for {field}: {value}")]
    InvalidValue { field: &'static str, value: i64 },

    #[error("unsupported frame format")]
    UnsupportedFormat,
}
