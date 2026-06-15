//! File-transfer ASDU types (TypeIDs 120-126), per IEC 60870-5-5 §6.
//!
//! These ASDUs orchestrate the multi-step file-transfer dialogue. They are
//! independent of direction — both a controlling station (master) and a
//! controlled station (outstation) may emit any of them. The accompanying
//! [`crate::file_transfer`] state machine sequences them into a complete
//! transfer.
//!
//! ```text
//!   120  F_FR_NA_1   file ready
//!   121  F_SR_NA_1   section ready
//!   122  F_SC_NA_1   select / call / deactivate / delete
//!   123  F_LS_NA_1   last section / last segment with checksum
//!   124  F_AF_NA_1   acknowledge file / section
//!   125  F_SG_NA_1   segment data
//!   126  F_DR_TA_1   directory listing
//! ```

#![allow(non_camel_case_types)]

use bytes::{Buf, BufMut};

use crate::asdu::header::{decode_ioa, encode_ioa, AsduAddressing, Ioa, Vsq};
use crate::asdu::ie::Cp56Time2a;
use crate::asdu::payload::AsduPayload;
use crate::error::{Error, Result};

// ---------------------------------------------------------------------------
// Atomic file-transfer elements
// ---------------------------------------------------------------------------

/// Name of File (2 octets, little-endian). Identifies a file on the peer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameOfFile(pub u16);

impl NameOfFile {
    pub const LEN: usize = 2;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u16_le(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_u16_le()))
    }
}

/// Name of Section (2 octets, little-endian). `0` means "no section / whole file".
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NameOfSection(pub u16);

impl NameOfSection {
    pub const LEN: usize = 2;
    /// Convention: section number 0 refers to the file as a whole, used during
    /// SELECT / FILE_READY before any section has been chosen.
    pub const WHOLE_FILE: Self = Self(0);

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u16_le(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_u16_le()))
    }
}

/// Length of File (3 octets, little-endian). Range: 0..=2^24-1 bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LengthOfFile(pub u32);

impl LengthOfFile {
    pub const LEN: usize = 3;
    pub const MAX: u32 = 0x00FF_FFFF;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let v = self.0 & Self::MAX;
        buf.put_u8(v as u8);
        buf.put_u8((v >> 8) as u8);
        buf.put_u8((v >> 16) as u8);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b0 = buf.get_u8() as u32;
        let b1 = buf.get_u8() as u32;
        let b2 = buf.get_u8() as u32;
        Ok(Self(b0 | (b1 << 8) | (b2 << 16)))
    }
}

/// Length of Section (3 octets, little-endian). Range: 0..=2^24-1 bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LengthOfSection(pub u32);

impl LengthOfSection {
    pub const LEN: usize = 3;
    pub const MAX: u32 = 0x00FF_FFFF;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let v = self.0 & Self::MAX;
        buf.put_u8(v as u8);
        buf.put_u8((v >> 8) as u8);
        buf.put_u8((v >> 16) as u8);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b0 = buf.get_u8() as u32;
        let b1 = buf.get_u8() as u32;
        let b2 = buf.get_u8() as u32;
        Ok(Self(b0 | (b1 << 8) | (b2 << 16)))
    }
}

// ---------------------------------------------------------------------------
// Qualifier octets
// ---------------------------------------------------------------------------

/// File Ready Qualifier (1 octet). Bit 7 selects positive/negative
/// acknowledgement; bits 0-6 carry an implementation-specific code.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Frq {
    /// Implementation-specific 7-bit qualifier code (0..=127).
    pub code: u8,
    /// `true` for negative ack ("file not ready").
    pub negative: bool,
}

impl Frq {
    pub const LEN: usize = 1;
    /// Standard "file ready, default acknowledgement" qualifier.
    pub const READY: Self = Self {
        code: 0,
        negative: false,
    };
    /// Standard "file not ready" negative acknowledgement.
    pub const NOT_READY: Self = Self {
        code: 0,
        negative: true,
    };

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.code & 0x7F;
        if self.negative {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            code: b & 0x7F,
            negative: b & 0x80 != 0,
        })
    }
}

/// Section Ready Qualifier (1 octet). Same layout as [`Frq`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Srq {
    pub code: u8,
    pub negative: bool,
}

impl Srq {
    pub const LEN: usize = 1;
    pub const READY: Self = Self {
        code: 0,
        negative: false,
    };
    pub const NOT_READY: Self = Self {
        code: 0,
        negative: true,
    };

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.code & 0x7F;
        if self.negative {
            b |= 0x80;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            code: b & 0x7F,
            negative: b & 0x80 != 0,
        })
    }
}

/// Select-and-Call Qualifier (1 octet). Low nibble selects the action,
/// high nibble carries the reason / status code.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scq {
    /// `UI4.1` — action (0..=15).
    pub action: ScqAction,
    /// `UI4.2` — vendor-defined status / reason (0..=15).
    pub status: u8,
}

/// Select-and-Call action codes (low nibble of [`Scq`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScqAction {
    #[default]
    Default,
    SelectFile,
    RequestFile,
    DeactivateFile,
    DeleteFile,
    SelectSection,
    RequestSection,
    DeactivateSection,
    /// 8..=15 are reserved for compatible extensions; the raw nibble is preserved.
    Other(u8),
}

impl ScqAction {
    fn to_bits(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::SelectFile => 1,
            Self::RequestFile => 2,
            Self::DeactivateFile => 3,
            Self::DeleteFile => 4,
            Self::SelectSection => 5,
            Self::RequestSection => 6,
            Self::DeactivateSection => 7,
            Self::Other(b) => b & 0x0F,
        }
    }

    fn from_bits(b: u8) -> Self {
        match b & 0x0F {
            0 => Self::Default,
            1 => Self::SelectFile,
            2 => Self::RequestFile,
            3 => Self::DeactivateFile,
            4 => Self::DeleteFile,
            5 => Self::SelectSection,
            6 => Self::RequestSection,
            7 => Self::DeactivateSection,
            n => Self::Other(n),
        }
    }
}

impl Scq {
    pub const LEN: usize = 1;

    pub fn new(action: ScqAction, status: u8) -> Self {
        Self {
            action,
            status: status & 0x0F,
        }
    }

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8((self.action.to_bits() & 0x0F) | ((self.status & 0x0F) << 4));
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            action: ScqAction::from_bits(b & 0x0F),
            status: (b >> 4) & 0x0F,
        })
    }
}

/// Last Section / Last Segment Qualifier (1 octet).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lsq {
    #[default]
    Default,
    /// Section transfer completed without deactivation.
    SectionWithoutDeactivate,
    /// Section transfer completed with deactivation.
    SectionWithDeactivate,
    /// File transfer completed without deactivation.
    FileWithoutDeactivate,
    /// File transfer completed with deactivation.
    FileWithDeactivate,
    /// 5..=255 are reserved / private use; preserved verbatim.
    Other(u8),
}

impl Lsq {
    pub const LEN: usize = 1;

    fn to_bits(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::SectionWithoutDeactivate => 1,
            Self::SectionWithDeactivate => 2,
            Self::FileWithoutDeactivate => 3,
            Self::FileWithDeactivate => 4,
            Self::Other(b) => b,
        }
    }

    fn from_bits(b: u8) -> Self {
        match b {
            0 => Self::Default,
            1 => Self::SectionWithoutDeactivate,
            2 => Self::SectionWithDeactivate,
            3 => Self::FileWithoutDeactivate,
            4 => Self::FileWithDeactivate,
            n => Self::Other(n),
        }
    }

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8(self.to_bits());
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self::from_bits(buf.get_u8()))
    }
}

/// Acknowledge-File / Acknowledge-Section Qualifier (1 octet). Low nibble
/// holds the action ack, high nibble holds the reason code.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Afq {
    /// `UI4.1` — acknowledgement code (0..=15).
    pub action: AfqAction,
    /// `UI4.2` — reason / error code (0..=15).
    pub status: u8,
}

/// Acknowledge action codes (low nibble of [`Afq`]).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AfqAction {
    #[default]
    Default,
    PositiveFile,
    NegativeFile,
    PositiveSection,
    NegativeSection,
    /// 5..=15 reserved / private use.
    Other(u8),
}

impl AfqAction {
    fn to_bits(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::PositiveFile => 1,
            Self::NegativeFile => 2,
            Self::PositiveSection => 3,
            Self::NegativeSection => 4,
            Self::Other(b) => b & 0x0F,
        }
    }

    fn from_bits(b: u8) -> Self {
        match b & 0x0F {
            0 => Self::Default,
            1 => Self::PositiveFile,
            2 => Self::NegativeFile,
            3 => Self::PositiveSection,
            4 => Self::NegativeSection,
            n => Self::Other(n),
        }
    }
}

impl Afq {
    pub const LEN: usize = 1;

    pub fn new(action: AfqAction, status: u8) -> Self {
        Self {
            action,
            status: status & 0x0F,
        }
    }

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8((self.action.to_bits() & 0x0F) | ((self.status & 0x0F) << 4));
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            action: AfqAction::from_bits(b & 0x0F),
            status: (b >> 4) & 0x0F,
        })
    }
}

/// Status of File (1 octet) — appears in directory entries (F_DR_TA_1).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sof {
    /// Bits 0-4 — application-defined status code.
    pub status: u8,
    /// `LFD` — this entry is the last file in the directory.
    pub last_file: bool,
    /// `FOR` — entry refers to a sub-directory rather than a file.
    pub sub_directory: bool,
    /// `FA` — file transfer is currently active.
    pub active: bool,
}

impl Sof {
    pub const LEN: usize = 1;
    const LFD: u8 = 0x20;
    const FOR_BIT: u8 = 0x40;
    const FA: u8 = 0x80;
    const STATUS_MASK: u8 = 0x1F;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        let mut b = self.status & Self::STATUS_MASK;
        if self.last_file {
            b |= Self::LFD;
        }
        if self.sub_directory {
            b |= Self::FOR_BIT;
        }
        if self.active {
            b |= Self::FA;
        }
        buf.put_u8(b);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let b = buf.get_u8();
        Ok(Self {
            status: b & Self::STATUS_MASK,
            last_file: b & Self::LFD != 0,
            sub_directory: b & Self::FOR_BIT != 0,
            active: b & Self::FA != 0,
        })
    }
}

/// Checksum (1 octet) — modulo-256 sum of all section data bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Checksum(pub u8);

impl Checksum {
    pub const LEN: usize = 1;

    /// Compute the modulo-256 checksum of `data`.
    pub fn of(data: &[u8]) -> Self {
        let mut acc: u8 = 0;
        for b in data {
            acc = acc.wrapping_add(*b);
        }
        Self(acc)
    }

    /// Incremental update — fold one byte into a running checksum.
    pub fn update(&mut self, byte: u8) {
        self.0 = self.0.wrapping_add(byte);
    }

    /// Incremental update from a slice.
    pub fn update_slice(&mut self, data: &[u8]) {
        for b in data {
            self.0 = self.0.wrapping_add(*b);
        }
    }

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        buf.put_u8(self.0);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        Ok(Self(buf.get_u8()))
    }
}

// ---------------------------------------------------------------------------
// F_FR_NA_1 (TypeID 120) — File Ready
// ---------------------------------------------------------------------------

/// `F_FR_NA_1` — sender announces a file is ready for transfer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct F_FR_NA_1 {
    pub ioa: Ioa,
    pub nof: NameOfFile,
    pub lof: LengthOfFile,
    pub frq: Frq,
}

impl AsduPayload for F_FR_NA_1 {
    const TYPE_ID: u8 = 120;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.nof.encode(buf);
        self.lof.encode(buf);
        self.frq.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let nof = NameOfFile::decode(buf)?;
        let lof = LengthOfFile::decode(buf)?;
        let frq = Frq::decode(buf)?;
        Ok(Self { ioa, nof, lof, frq })
    }
}

// ---------------------------------------------------------------------------
// F_SR_NA_1 (TypeID 121) — Section Ready
// ---------------------------------------------------------------------------

/// `F_SR_NA_1` — sender announces a section is ready for transfer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct F_SR_NA_1 {
    pub ioa: Ioa,
    pub nof: NameOfFile,
    pub nos: NameOfSection,
    pub los: LengthOfSection,
    pub srq: Srq,
}

impl AsduPayload for F_SR_NA_1 {
    const TYPE_ID: u8 = 121;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.nof.encode(buf);
        self.nos.encode(buf);
        self.los.encode(buf);
        self.srq.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let nof = NameOfFile::decode(buf)?;
        let nos = NameOfSection::decode(buf)?;
        let los = LengthOfSection::decode(buf)?;
        let srq = Srq::decode(buf)?;
        Ok(Self {
            ioa,
            nof,
            nos,
            los,
            srq,
        })
    }
}

// ---------------------------------------------------------------------------
// F_SC_NA_1 (TypeID 122) — Select / Call / Deactivate / Delete
// ---------------------------------------------------------------------------

/// `F_SC_NA_1` — direct the peer to select, request, deactivate or delete a
/// file or section.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct F_SC_NA_1 {
    pub ioa: Ioa,
    pub nof: NameOfFile,
    pub nos: NameOfSection,
    pub scq: Scq,
}

impl AsduPayload for F_SC_NA_1 {
    const TYPE_ID: u8 = 122;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.nof.encode(buf);
        self.nos.encode(buf);
        self.scq.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let nof = NameOfFile::decode(buf)?;
        let nos = NameOfSection::decode(buf)?;
        let scq = Scq::decode(buf)?;
        Ok(Self { ioa, nof, nos, scq })
    }
}

// ---------------------------------------------------------------------------
// F_LS_NA_1 (TypeID 123) — Last Section / Last Segment with checksum
// ---------------------------------------------------------------------------

/// `F_LS_NA_1` — close a section or file with its checksum.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct F_LS_NA_1 {
    pub ioa: Ioa,
    pub nof: NameOfFile,
    pub nos: NameOfSection,
    pub lsq: Lsq,
    pub chs: Checksum,
}

impl AsduPayload for F_LS_NA_1 {
    const TYPE_ID: u8 = 123;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.nof.encode(buf);
        self.nos.encode(buf);
        self.lsq.encode(buf);
        self.chs.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let nof = NameOfFile::decode(buf)?;
        let nos = NameOfSection::decode(buf)?;
        let lsq = Lsq::decode(buf)?;
        let chs = Checksum::decode(buf)?;
        Ok(Self {
            ioa,
            nof,
            nos,
            lsq,
            chs,
        })
    }
}

// ---------------------------------------------------------------------------
// F_AF_NA_1 (TypeID 124) — Acknowledge File / Section
// ---------------------------------------------------------------------------

/// `F_AF_NA_1` — acknowledge (positively or negatively) a file or section
/// transfer.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct F_AF_NA_1 {
    pub ioa: Ioa,
    pub nof: NameOfFile,
    pub nos: NameOfSection,
    pub afq: Afq,
}

impl AsduPayload for F_AF_NA_1 {
    const TYPE_ID: u8 = 124;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.nof.encode(buf);
        self.nos.encode(buf);
        self.afq.encode(buf);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let nof = NameOfFile::decode(buf)?;
        let nos = NameOfSection::decode(buf)?;
        let afq = Afq::decode(buf)?;
        Ok(Self { ioa, nof, nos, afq })
    }
}

// ---------------------------------------------------------------------------
// F_SG_NA_1 (TypeID 125) — Segment of section data
// ---------------------------------------------------------------------------

/// Maximum length of the segment payload carried by `F_SG_NA_1`. The standard
/// uses an 8-bit Length-of-Segment field, so the payload is capped at 255
/// bytes. Implementations typically stay below 240 to fit within IEC 60870-5
/// frame budgets.
pub const MAX_SEGMENT_BYTES: usize = 0xFF;

/// `F_SG_NA_1` — one segment of a section. The segment data length is given
/// by an explicit 1-octet field on the wire (it is NOT derivable from the
/// surrounding frame size).
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct F_SG_NA_1 {
    pub ioa: Ioa,
    pub nof: NameOfFile,
    pub nos: NameOfSection,
    /// Segment data. Must be `<= MAX_SEGMENT_BYTES` bytes long.
    pub segment: Vec<u8>,
}

impl AsduPayload for F_SG_NA_1 {
    const TYPE_ID: u8 = 125;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        encode_ioa(buf, self.ioa, addressing.ioa_size);
        self.nof.encode(buf);
        self.nos.encode(buf);
        let len = core::cmp::min(self.segment.len(), MAX_SEGMENT_BYTES) as u8;
        buf.put_u8(len);
        buf.put_slice(&self.segment[..len as usize]);
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        _vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let ioa = decode_ioa(buf, addressing.ioa_size)?;
        let nof = NameOfFile::decode(buf)?;
        let nos = NameOfSection::decode(buf)?;
        ensure(buf, 1)?;
        let len = buf.get_u8() as usize;
        ensure(buf, len)?;
        let mut segment = vec![0u8; len];
        buf.copy_to_slice(&mut segment);
        Ok(Self {
            ioa,
            nof,
            nos,
            segment,
        })
    }
}

// ---------------------------------------------------------------------------
// F_DR_TA_1 (TypeID 126) — Directory listing
// ---------------------------------------------------------------------------

/// A single directory entry as carried inside `F_DR_TA_1`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DirectoryEntry {
    pub nof: NameOfFile,
    pub lof: LengthOfFile,
    pub sof: Sof,
    pub time: Cp56Time2a,
}

impl DirectoryEntry {
    pub const LEN: usize = NameOfFile::LEN + LengthOfFile::LEN + Sof::LEN + Cp56Time2a::LEN;

    pub fn encode<B: BufMut>(self, buf: &mut B) {
        self.nof.encode(buf);
        self.lof.encode(buf);
        self.sof.encode(buf);
        self.time.encode(buf);
    }

    pub fn decode<B: Buf>(buf: &mut B) -> Result<Self> {
        ensure(buf, Self::LEN)?;
        let nof = NameOfFile::decode(buf)?;
        let lof = LengthOfFile::decode(buf)?;
        let sof = Sof::decode(buf)?;
        let time = Cp56Time2a::decode(buf)?;
        Ok(Self {
            nof,
            lof,
            sof,
            time,
        })
    }
}

/// `F_DR_TA_1` — directory listing: zero or more `(IOA, DirectoryEntry)` pairs.
///
/// Each entry carries its own IOA; the count comes from the VSQ. Use
/// [`Vsq::single`] to build the ASDU (the standard does not assign a sequence
/// meaning to a directory listing).
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct F_DR_TA_1 {
    pub entries: Vec<(Ioa, DirectoryEntry)>,
}

impl AsduPayload for F_DR_TA_1 {
    const TYPE_ID: u8 = 126;
    fn encode_information_objects<B: BufMut>(
        &self,
        buf: &mut B,
        vsq: Vsq,
        addressing: AsduAddressing,
    ) {
        crate::asdu::io_list::encode_io_list(buf, &self.entries, vsq, addressing, |b, entry| {
            entry.encode(b)
        });
    }
    fn decode_information_objects<B: Buf>(
        buf: &mut B,
        vsq: Vsq,
        addressing: AsduAddressing,
    ) -> Result<Self> {
        let entries = crate::asdu::io_list::decode_io_list(buf, vsq, addressing, |b| {
            DirectoryEntry::decode(b)
        })?;
        Ok(Self { entries })
    }
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn ensure<B: Buf>(buf: &B, n: usize) -> Result<()> {
    if buf.remaining() < n {
        Err(Error::Incomplete {
            needed: n,
            have: buf.remaining(),
        })
    } else {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asdu::cot::{Cause, Cot};
    use crate::asdu::envelope::Asdu;
    use crate::asdu::header::CommonAddress;
    use bytes::BytesMut;
    use proptest::prelude::*;

    fn roundtrip_iec104<P: AsduPayload + PartialEq + core::fmt::Debug + Clone>(
        payload: P,
        vsq: Vsq,
        cause: Cause,
    ) {
        let asdu = Asdu::from_payload(
            Cot::with(cause),
            CommonAddress(1),
            vsq,
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        assert_eq!(parsed.type_id(), P::TYPE_ID);
        let decoded: P = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(decoded, payload);
    }

    // ---- Atomic element byte-layout tests ----

    #[test]
    fn name_of_file_little_endian() {
        let mut buf = BytesMut::new();
        NameOfFile(0xBEEF).encode(&mut buf);
        assert_eq!(&buf[..], &[0xEF, 0xBE]);
    }

    #[test]
    fn length_of_file_3_octet_little_endian() {
        let mut buf = BytesMut::new();
        LengthOfFile(0x12_3456).encode(&mut buf);
        assert_eq!(&buf[..], &[0x56, 0x34, 0x12]);
        let mut slice: &[u8] = &buf;
        assert_eq!(LengthOfFile::decode(&mut slice).unwrap().0, 0x12_3456);
    }

    #[test]
    fn length_of_file_masks_upper_byte_on_encode() {
        let mut buf = BytesMut::new();
        LengthOfFile(0xDEAD_BEEF).encode(&mut buf);
        // Only the low 24 bits should be emitted.
        assert_eq!(&buf[..], &[0xEF, 0xBE, 0xAD]);
    }

    #[test]
    fn checksum_modulo_256() {
        // 0xFE + 0x03 = 0x0101 -> mod 256 = 0x01
        assert_eq!(Checksum::of(&[0xFE, 0x03]).0, 0x01);
        // Compare incremental vs bulk
        let mut c = Checksum::default();
        for b in [0x10u8, 0x20, 0x30, 0xFF, 0xFF] {
            c.update(b);
        }
        assert_eq!(c.0, Checksum::of(&[0x10, 0x20, 0x30, 0xFF, 0xFF]).0);
    }

    #[test]
    fn frq_bit7_is_negative_flag() {
        let mut buf = BytesMut::new();
        Frq {
            code: 0x12,
            negative: true,
        }
        .encode(&mut buf);
        assert_eq!(&buf[..], &[0x92]);
        let mut slice: &[u8] = &buf;
        let decoded = Frq::decode(&mut slice).unwrap();
        assert_eq!(decoded.code, 0x12);
        assert!(decoded.negative);
    }

    #[test]
    fn scq_low_nibble_action_high_nibble_status() {
        let mut buf = BytesMut::new();
        Scq::new(ScqAction::RequestFile, 3).encode(&mut buf);
        // action=2, status=3 → 0x32
        assert_eq!(&buf[..], &[0x32]);
        let mut slice: &[u8] = &buf;
        let decoded = Scq::decode(&mut slice).unwrap();
        assert!(matches!(decoded.action, ScqAction::RequestFile));
        assert_eq!(decoded.status, 3);
    }

    #[test]
    fn sof_status_bits_pack_correctly() {
        let mut buf = BytesMut::new();
        Sof {
            status: 0x0A,
            last_file: true,
            sub_directory: false,
            active: true,
        }
        .encode(&mut buf);
        // 0x80 (FA) | 0x20 (LFD) | 0x0A (status) = 0xAA
        assert_eq!(&buf[..], &[0xAA]);
    }

    #[test]
    fn lsq_known_codes_roundtrip() {
        for v in [
            Lsq::Default,
            Lsq::SectionWithoutDeactivate,
            Lsq::SectionWithDeactivate,
            Lsq::FileWithoutDeactivate,
            Lsq::FileWithDeactivate,
            Lsq::Other(0x55),
        ] {
            let mut buf = BytesMut::new();
            v.encode(&mut buf);
            let mut slice: &[u8] = &buf;
            let decoded = Lsq::decode(&mut slice).unwrap();
            assert_eq!(v, decoded, "for {v:?}");
        }
    }

    // ---- ASDU round-trips ----

    #[test]
    fn f_fr_na_1_roundtrip() {
        roundtrip_iec104(
            F_FR_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(0x1234),
                lof: LengthOfFile(0x00_4242),
                frq: Frq {
                    code: 0,
                    negative: false,
                },
            },
            Vsq::single(1),
            Cause::FILE_TRANSFER,
        );
    }

    #[test]
    fn f_sr_na_1_roundtrip() {
        roundtrip_iec104(
            F_SR_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(0x1234),
                nos: NameOfSection(7),
                los: LengthOfSection(2048),
                srq: Srq::READY,
            },
            Vsq::single(1),
            Cause::FILE_TRANSFER,
        );
    }

    #[test]
    fn f_sc_na_1_roundtrip() {
        roundtrip_iec104(
            F_SC_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(0x1234),
                nos: NameOfSection(0),
                scq: Scq::new(ScqAction::SelectFile, 0),
            },
            Vsq::single(1),
            Cause::FILE_TRANSFER,
        );
    }

    #[test]
    fn f_ls_na_1_roundtrip() {
        roundtrip_iec104(
            F_LS_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(0x1234),
                nos: NameOfSection(1),
                lsq: Lsq::FileWithoutDeactivate,
                chs: Checksum(0xA5),
            },
            Vsq::single(1),
            Cause::FILE_TRANSFER,
        );
    }

    #[test]
    fn f_af_na_1_roundtrip() {
        roundtrip_iec104(
            F_AF_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(0x1234),
                nos: NameOfSection(1),
                afq: Afq::new(AfqAction::PositiveSection, 0),
            },
            Vsq::single(1),
            Cause::FILE_TRANSFER,
        );
    }

    #[test]
    fn f_sg_na_1_roundtrip_short_payload() {
        roundtrip_iec104(
            F_SG_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(0x1234),
                nos: NameOfSection(1),
                segment: vec![0xDE, 0xAD, 0xBE, 0xEF],
            },
            Vsq::single(1),
            Cause::FILE_TRANSFER,
        );
    }

    #[test]
    fn f_sg_na_1_truncates_oversized_segment_on_encode() {
        let payload = F_SG_NA_1 {
            ioa: Ioa(0),
            nof: NameOfFile(0),
            nos: NameOfSection(0),
            segment: vec![0x42; 300], // > MAX_SEGMENT_BYTES (255)
        };
        let asdu = Asdu::from_payload(
            Cot::with(Cause::FILE_TRANSFER),
            CommonAddress(1),
            Vsq::single(1),
            &payload,
            AsduAddressing::IEC104,
        );
        let mut buf = BytesMut::new();
        asdu.encode(&mut buf, AsduAddressing::IEC104);
        let mut slice: &[u8] = &buf;
        let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
        let decoded: F_SG_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
        assert_eq!(decoded.segment.len(), MAX_SEGMENT_BYTES);
    }

    #[test]
    fn f_dr_ta_1_roundtrip_multi_entry() {
        let entry_a = DirectoryEntry {
            nof: NameOfFile(1),
            lof: LengthOfFile(100),
            sof: Sof {
                status: 1,
                ..Default::default()
            },
            time: Cp56Time2a::default(),
        };
        let entry_b = DirectoryEntry {
            nof: NameOfFile(2),
            lof: LengthOfFile(2_000_000),
            sof: Sof {
                last_file: true,
                ..Default::default()
            },
            time: Cp56Time2a::default(),
        };
        roundtrip_iec104(
            F_DR_TA_1 {
                entries: vec![(Ioa(0), entry_a), (Ioa(1), entry_b)],
            },
            Vsq::single(2),
            Cause::SPONTANEOUS,
        );
    }

    // ---- Property-based round-trips ----

    proptest! {
        #[test]
        fn prop_length_of_file_u24_roundtrip(v in 0u32..=LengthOfFile::MAX) {
            let mut buf = BytesMut::new();
            LengthOfFile(v).encode(&mut buf);
            let mut slice: &[u8] = &buf;
            prop_assert_eq!(LengthOfFile::decode(&mut slice).unwrap().0, v);
        }

        #[test]
        fn prop_length_of_section_u24_roundtrip(v in 0u32..=LengthOfSection::MAX) {
            let mut buf = BytesMut::new();
            LengthOfSection(v).encode(&mut buf);
            let mut slice: &[u8] = &buf;
            prop_assert_eq!(LengthOfSection::decode(&mut slice).unwrap().0, v);
        }

        #[test]
        fn prop_segment_length_prefix_matches(data in proptest::collection::vec(any::<u8>(), 0..=MAX_SEGMENT_BYTES)) {
            let payload = F_SG_NA_1 {
                ioa: Ioa(0),
                nof: NameOfFile(42),
                nos: NameOfSection(1),
                segment: data.clone(),
            };
            let asdu = Asdu::from_payload(
                Cot::with(Cause::FILE_TRANSFER),
                CommonAddress(1),
                Vsq::single(1),
                &payload,
                AsduAddressing::IEC104,
            );
            let mut buf = BytesMut::new();
            asdu.encode(&mut buf, AsduAddressing::IEC104);
            let mut slice: &[u8] = &buf;
            let parsed = Asdu::decode(&mut slice, AsduAddressing::IEC104).unwrap();
            let decoded: F_SG_NA_1 = parsed.decode_payload(AsduAddressing::IEC104).unwrap();
            prop_assert_eq!(decoded.segment, data);
        }

        #[test]
        fn prop_checksum_independent_of_order(mut data in proptest::collection::vec(any::<u8>(), 0..200)) {
            let direct = Checksum::of(&data);
            // Reverse should yield the same sum (addition is commutative).
            data.reverse();
            prop_assert_eq!(Checksum::of(&data).0, direct.0);
        }
    }
}
