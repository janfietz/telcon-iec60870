//! Per-IOA hysteresis / deadband evaluator for spontaneous emissions.
//!
//! [`DeadbandTracker`] is sans-I/O: it owns a policy table and a
//! last-emitted snapshot per IOA. The caller drives it via two methods:
//!
//! * [`DeadbandTracker::observe`] — called for **every** outgoing ASDU
//!   carrying a value (GI response, explicit Set, etc.). Refreshes the
//!   baseline.
//! * [`DeadbandTracker::evaluate`] — called by the spontaneous-candidate
//!   path. Returns [`EmitDecision::Emit`] when the new sample crosses the
//!   threshold, quality changed, or there is no baseline; otherwise
//!   [`EmitDecision::Suppress`].
//!
//! Default for an unregistered IOA is [`DeadbandPolicy::None`] — every
//! observation emits. Thresholds must be opted into explicitly.

use std::collections::HashMap;

use iec60870_proto::asdu::ie::{DoublePoint, Qds};
use iec60870_proto::asdu::Ioa;
use thiserror::Error;

/// What changes count as a real change for one point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeadbandPolicy {
    /// No deadband — every observation emits.
    None,
    /// Emit when `|new − last_emitted| ≥ delta`. For Single/Double, any
    /// transition counts; `delta` is ignored.
    Absolute { delta: f64 },
    /// Emit when `|new − last_emitted| ≥ (pct/100) * max(|last|, floor)`.
    /// `floor` prevents divide-by-zero degeneracy near zero. For
    /// Single/Double, any transition counts; `pct` and `floor` are ignored.
    Percent { pct: f32, floor: f64 },
}

/// Kind-agnostic value carried by every monitored type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MonitoredValue {
    Single(bool),
    Double(DoublePoint),
    /// Normalized value in [-1.0, 1.0].
    Normalized(f32),
    Scaled(i16),
    Float(f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Single,
    Double,
    Normalized,
    Scaled,
    Float,
}

impl MonitoredValue {
    pub fn kind(self) -> ValueKind {
        match self {
            MonitoredValue::Single(_) => ValueKind::Single,
            MonitoredValue::Double(_) => ValueKind::Double,
            MonitoredValue::Normalized(_) => ValueKind::Normalized,
            MonitoredValue::Scaled(_) => ValueKind::Scaled,
            MonitoredValue::Float(_) => ValueKind::Float,
        }
    }

    /// `|value|` as f64. Returns 0.0 for Single/Double (which never reach
    /// the magnitude-based comparator).
    fn magnitude(self) -> f64 {
        match self {
            MonitoredValue::Single(_) | MonitoredValue::Double(_) => 0.0,
            MonitoredValue::Normalized(f) | MonitoredValue::Float(f) => f64::from(f).abs(),
            MonitoredValue::Scaled(i) => f64::from(i).abs(),
        }
    }
}

/// Outcome of [`DeadbandTracker::evaluate`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EmitDecision {
    /// Threshold crossed, quality differs, or first-ever sample. The
    /// tracker's baseline has been updated to the new snapshot.
    Emit,
    /// Within deadband and quality unchanged. Baseline untouched.
    Suppress,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum DeadbandError {
    #[error("IOA {ioa:?} baseline is {expected:?} but observation is {actual:?}")]
    KindMismatch {
        ioa: Ioa,
        expected: ValueKind,
        actual: ValueKind,
    },
}

/// Per-IOA last-emitted snapshot + policy store.
#[derive(Default, Debug)]
pub struct DeadbandTracker {
    policies: HashMap<Ioa, DeadbandPolicy>,
    baselines: HashMap<Ioa, Baseline>,
}

#[derive(Clone, Copy, Debug)]
struct Baseline {
    value: MonitoredValue,
    quality: Qds,
}

impl DeadbandTracker {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iec60870_proto::asdu::ie::Quality;

    fn qds() -> Qds {
        Qds {
            overflow: false,
            quality: Quality::default(),
        }
    }

    #[test]
    fn first_sample_emits() {
        let mut t = DeadbandTracker::new();
        let decision = t
            .evaluate(Ioa(10), MonitoredValue::Float(1.0), qds())
            .expect("evaluate ok");
        assert_eq!(decision, EmitDecision::Emit);
    }
}
