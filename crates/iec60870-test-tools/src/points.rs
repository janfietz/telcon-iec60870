//! Server-side process image.
//!
//! Holds the IOA-keyed map of typed values, their quality bits, the most
//! recent timestamp, and the per-IOA simulator schedule. Used by both the
//! control handler (for `get`/`set`/`list`/`sim_*` ops) and the interrogation
//! responder (to render every monitored point as an outbound ASDU).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::BytesMut;
use iec60870::proto::asdu::cot::Cot;
use iec60870::proto::asdu::header::{AsduAddressing, CommonAddress, Ioa, Vsq};
use iec60870::proto::asdu::ie::{
    Cp56Time2a, Diq, DoublePoint, Nva, Qds, Quality, R32, Siq, Sva,
};
use iec60870::proto::asdu::types::{
    M_DP_NA_1, M_DP_TB_1, M_ME_NA_1, M_ME_NB_1, M_ME_NC_1, M_ME_TD_1, M_ME_TE_1, M_ME_TF_1,
    M_SP_NA_1, M_SP_TB_1,
};
use iec60870::proto::asdu::{Asdu, AsduPayload};

use crate::wire::{DoublePointWire, PointKind, PointValue, QualityWire, SimSchedule};

/// Build a `Cp56Time2a` from the current wall-clock using only `std`.
///
/// The year is stored raw (0-99), corresponding to 2000-2099.
pub fn now_cp56() -> Cp56Time2a {
    // Seconds since Unix epoch.
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let subsec_ms = (dur.subsec_millis()) as u16;

    // Break into civil time (UTC). Adapted from the classic algorithm.
    // Days since epoch.
    let days = total_secs / 86400;
    let secs_of_day = total_secs % 86400;

    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    let second = (secs_of_day % 60) as u8;
    let milliseconds = second as u16 * 1000 + subsec_ms;

    // Gregorian calendar from Julian Day Number.
    // JDN for 1970-01-01 is 2440588.
    let jdn = days + 2_440_588;
    let a = jdn + 32_044;
    let b = (4 * a + 3) / 146_097;
    let c = a - (146_097 * b) / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - (1461 * d) / 4;
    let m = (5 * e + 2) / 153;

    let day = (e - (153 * m + 2) / 5 + 1) as u8;
    let month = (m + 3 - 12 * (m / 10)) as u8;
    let year_full = 100 * b + d - 4800 + m / 10;
    let year = (year_full % 100) as u8;

    // Day of week: JDN mod 7 gives 0=Mon..6=Sun (per IEC: 1=Mon, 7=Sun).
    let dow = ((jdn + 1) % 7) as u8; // 0..6
    let day_of_week = if dow == 0 { 7u8 } else { dow };

    Cp56Time2a {
        milliseconds,
        minute,
        hour,
        day,
        day_of_week,
        month,
        year,
        summer_time: false,
        invalid: false,
        genuine: false,
    }
}

// ---------------------------------------------------------------------------
// Quality conversion
// ---------------------------------------------------------------------------

pub fn quality_from_wire(q: QualityWire) -> Quality {
    Quality {
        blocked: q.blocked,
        substituted: q.substituted,
        not_topical: q.not_topical,
        invalid: q.invalid,
    }
}

pub fn quality_to_wire(q: Quality) -> QualityWire {
    QualityWire {
        overflow: false,
        blocked: q.blocked,
        substituted: q.substituted,
        not_topical: q.not_topical,
        invalid: q.invalid,
    }
}

pub fn qds_to_wire(qds: Qds) -> QualityWire {
    QualityWire {
        overflow: qds.overflow,
        blocked: qds.quality.blocked,
        substituted: qds.quality.substituted,
        not_topical: qds.quality.not_topical,
        invalid: qds.quality.invalid,
    }
}

pub fn qds_from_wire(q: QualityWire) -> Qds {
    Qds {
        overflow: q.overflow,
        quality: Quality {
            blocked: q.blocked,
            substituted: q.substituted,
            not_topical: q.not_topical,
            invalid: q.invalid,
        },
    }
}

fn dp_from_wire(dpw: DoublePointWire) -> DoublePoint {
    match dpw {
        DoublePointWire::Intermediate => DoublePoint::Intermediate,
        DoublePointWire::Off => DoublePoint::Off,
        DoublePointWire::On => DoublePoint::On,
        DoublePointWire::Indeterminate => DoublePoint::Indeterminate,
    }
}

// ---------------------------------------------------------------------------
// Point entry
// ---------------------------------------------------------------------------

/// One monitored data point in the process image.
#[derive(Clone, Debug)]
pub struct PointEntry {
    pub kind: PointKind,
    pub value: PointValue,
    pub quality: QualityWire,
    pub schedule: SimSchedule,
}

impl PointEntry {
    /// Construct an entry with its default initial value and schedule.
    pub fn new(kind: PointKind, schedule: SimSchedule) -> Self {
        let value = match kind {
            PointKind::SpNa | PointKind::SpTb => PointValue::Single(false),
            PointKind::DpNa | PointKind::DpTb => PointValue::Double(DoublePointWire::Off),
            PointKind::MeNa | PointKind::MeTd => PointValue::Normalized(0.0),
            PointKind::MeNb | PointKind::MeTe => PointValue::Scaled(0),
            PointKind::MeNc | PointKind::MeTf => PointValue::Float(0.0),
        };
        Self {
            kind,
            value,
            quality: QualityWire::default(),
            schedule,
        }
    }
}

// ---------------------------------------------------------------------------
// Process image
// ---------------------------------------------------------------------------

/// The full server process image, keyed by IOA.
#[derive(Default, Debug)]
pub struct ProcessImage {
    points: HashMap<u32, PointEntry>,
}

impl ProcessImage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, ioa: u32, entry: PointEntry) {
        self.points.insert(ioa, entry);
    }

    pub fn get(&self, ioa: u32) -> Option<&PointEntry> {
        self.points.get(&ioa)
    }

    pub fn get_mut(&mut self, ioa: u32) -> Option<&mut PointEntry> {
        self.points.get_mut(&ioa)
    }

    /// Update or insert a point's value and quality.
    pub fn set(&mut self, ioa: u32, value: PointValue, quality: Option<QualityWire>) -> bool {
        if let Some(entry) = self.points.get_mut(&ioa) {
            entry.value = value;
            if let Some(q) = quality {
                entry.quality = q;
            }
            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Iterate all IOAs (unsorted).
    pub fn iter(&self) -> impl Iterator<Item = (u32, &PointEntry)> {
        self.points.iter().map(|(&ioa, e)| (ioa, e))
    }

    /// Iterate only IOAs of a specific `PointKind`.
    pub fn iter_kind(&self, kind: PointKind) -> impl Iterator<Item = (u32, &PointEntry)> {
        self.points
            .iter()
            .filter(move |(_, e)| e.kind == kind)
            .map(|(&ioa, e)| (ioa, e))
    }

    /// IOA lookup sorted ascending.
    pub fn sorted_ioas(&self) -> Vec<u32> {
        let mut ioas: Vec<u32> = self.points.keys().copied().collect();
        ioas.sort_unstable();
        ioas
    }
}

// ---------------------------------------------------------------------------
// ASDU encoding helpers
// ---------------------------------------------------------------------------

fn encode_asdu<P: AsduPayload>(payload: &P, cot: Cot, ca: CommonAddress) -> Vec<u8> {
    // All point-encoding callers supply exactly one object per ASDU.
    let vsq = Vsq::single(1);
    let asdu = Asdu::from_payload(cot, ca, vsq, payload, AsduAddressing::IEC104);
    let mut buf = BytesMut::new();
    asdu.encode(&mut buf, AsduAddressing::IEC104);
    buf.to_vec()
}

/// Build a single-IOA ASDU for a given point entry. Returns `None` if the
/// value doesn't match the kind (should never happen in a well-formed store).
pub fn encode_point(ioa: u32, entry: &PointEntry, cot: Cot, ca: CommonAddress) -> Option<Vec<u8>> {
    let ioa_ie = Ioa(ioa);
    let ts = now_cp56();
    let q = entry.quality;

    match entry.kind {
        PointKind::SpNa => {
            let on = matches!(entry.value, PointValue::Single(true));
            let payload = M_SP_NA_1 {
                objects: vec![(ioa_ie, Siq { on, quality: quality_from_wire(q) })],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::DpNa => {
            let state = match &entry.value {
                PointValue::Double(dpw) => dp_from_wire(*dpw),
                _ => DoublePoint::Off,
            };
            let payload = M_DP_NA_1 {
                objects: vec![(ioa_ie, Diq { state, quality: quality_from_wire(q) })],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::MeNa => {
            let v = match entry.value {
                PointValue::Normalized(f) => Nva::from_f32(f),
                _ => Nva(0),
            };
            let payload = M_ME_NA_1 {
                objects: vec![(ioa_ie, (v, qds_from_wire(q)))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::MeNb => {
            let v = match entry.value {
                PointValue::Scaled(s) => Sva(s),
                _ => Sva(0),
            };
            let payload = M_ME_NB_1 {
                objects: vec![(ioa_ie, (v, qds_from_wire(q)))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::MeNc => {
            let v = match entry.value {
                PointValue::Float(f) => R32(f),
                _ => R32(0.0),
            };
            let payload = M_ME_NC_1 {
                objects: vec![(ioa_ie, (v, qds_from_wire(q)))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::SpTb => {
            let on = matches!(entry.value, PointValue::Single(true));
            let payload = M_SP_TB_1 {
                objects: vec![(ioa_ie, (Siq { on, quality: quality_from_wire(q) }, ts))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::DpTb => {
            let state = match &entry.value {
                PointValue::Double(dpw) => dp_from_wire(*dpw),
                _ => DoublePoint::Off,
            };
            let payload = M_DP_TB_1 {
                objects: vec![(ioa_ie, (Diq { state, quality: quality_from_wire(q) }, ts))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::MeTd => {
            let v = match entry.value {
                PointValue::Normalized(f) => Nva::from_f32(f),
                _ => Nva(0),
            };
            let payload = M_ME_TD_1 {
                objects: vec![(ioa_ie, (v, qds_from_wire(q), ts))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::MeTe => {
            let v = match entry.value {
                PointValue::Scaled(s) => Sva(s),
                _ => Sva(0),
            };
            let payload = M_ME_TE_1 {
                objects: vec![(ioa_ie, (v, qds_from_wire(q), ts))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
        PointKind::MeTf => {
            let v = match entry.value {
                PointValue::Float(f) => R32(f),
                _ => R32(0.0),
            };
            let payload = M_ME_TF_1 {
                objects: vec![(ioa_ie, (v, qds_from_wire(q), ts))],
            };
            Some(encode_asdu(&payload, cot, ca))
        }
    }
}

/// Encode a `PointValue` as a JSON value for event streaming.
pub fn value_to_json(v: &PointValue) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

/// Convert a `PointValue` to the wire `PointValue` shape expected by `SimSchedule` ticks.
pub fn dp_wire_to_value(dpw: DoublePointWire) -> PointValue {
    PointValue::Double(dpw)
}

// ---------------------------------------------------------------------------
// Default IOA table
// ---------------------------------------------------------------------------

/// Populate the process image with the canonical IOA table.
pub fn populate_default(image: &mut ProcessImage) {
    // SpNa: IOAs 100-104, Toggle 5000 ms, phase i*1000
    for i in 0u32..5 {
        image.insert(
            100 + i,
            PointEntry::new(
                PointKind::SpNa,
                SimSchedule::Toggle {
                    interval_ms: 5000,
                    phase_ms: u64::from(i) * 1000,
                },
            ),
        );
    }
    // DpNa: IOAs 200-204, Rotate 7000 ms
    for i in 0u32..5 {
        image.insert(
            200 + i,
            PointEntry::new(PointKind::DpNa, SimSchedule::Rotate { interval_ms: 7000 }),
        );
    }
    // MeNa: IOAs 300-304, RandomWalk
    for i in 0u32..5 {
        image.insert(
            300 + i,
            PointEntry::new(
                PointKind::MeNa,
                SimSchedule::RandomWalk {
                    interval_ms: 1000,
                    step: 0.05,
                    min: -1.0,
                    max: 1.0,
                },
            ),
        );
    }
    // MeNb: IOAs 400-404, StepUp
    for i in 0u32..5 {
        image.insert(
            400 + i,
            PointEntry::new(
                PointKind::MeNb,
                SimSchedule::StepUp {
                    interval_ms: 2000,
                    step: 1,
                    wrap_at: 100,
                },
            ),
        );
    }
    // MeNc: IOAs 500-504, Sine
    for i in 0u32..5 {
        image.insert(
            500 + i,
            PointEntry::new(
                PointKind::MeNc,
                SimSchedule::Sine {
                    interval_ms: 500,
                    period_ms: 10000,
                    amplitude: 50.0,
                    offset: 0.0,
                },
            ),
        );
    }
    // SpTb: IOAs 1100-1104
    for i in 0u32..5 {
        image.insert(
            1100 + i,
            PointEntry::new(
                PointKind::SpTb,
                SimSchedule::Toggle {
                    interval_ms: 5000,
                    phase_ms: u64::from(i) * 1000,
                },
            ),
        );
    }
    // DpTb: IOAs 1200-1204
    for i in 0u32..5 {
        image.insert(
            1200 + i,
            PointEntry::new(PointKind::DpTb, SimSchedule::Rotate { interval_ms: 7000 }),
        );
    }
    // MeTd: IOAs 1300-1304
    for i in 0u32..5 {
        image.insert(
            1300 + i,
            PointEntry::new(
                PointKind::MeTd,
                SimSchedule::RandomWalk {
                    interval_ms: 1000,
                    step: 0.05,
                    min: -1.0,
                    max: 1.0,
                },
            ),
        );
    }
    // MeTe: IOAs 1400-1404
    for i in 0u32..5 {
        image.insert(
            1400 + i,
            PointEntry::new(
                PointKind::MeTe,
                SimSchedule::StepUp {
                    interval_ms: 2000,
                    step: 1,
                    wrap_at: 100,
                },
            ),
        );
    }
    // MeTf: IOAs 1500-1504
    for i in 0u32..5 {
        image.insert(
            1500 + i,
            PointEntry::new(
                PointKind::MeTf,
                SimSchedule::Sine {
                    interval_ms: 500,
                    period_ms: 10000,
                    amplitude: 50.0,
                    offset: 0.0,
                },
            ),
        );
    }
}

/// Map a group interrogation qualifier (Qoi 21..=36) to the `PointKind`
/// that belongs to that group in our IOA layout.
pub fn kind_for_group(group: u8) -> Option<PointKind> {
    match group {
        1 => Some(PointKind::SpNa),
        2 => Some(PointKind::DpNa),
        3 => Some(PointKind::MeNa),
        4 => Some(PointKind::MeNb),
        5 => Some(PointKind::MeNc),
        6 => Some(PointKind::SpTb),
        7 => Some(PointKind::DpTb),
        8 => Some(PointKind::MeTd),
        9 => Some(PointKind::MeTe),
        10 => Some(PointKind::MeTf),
        _ => None,
    }
}
