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

    pub fn set_policy(&mut self, ioa: Ioa, policy: DeadbandPolicy) {
        self.policies.insert(ioa, policy);
    }

    pub fn policy(&self, ioa: Ioa) -> DeadbandPolicy {
        self.policies.get(&ioa).copied().unwrap_or(DeadbandPolicy::None)
    }

    pub fn remove_policy(&mut self, ioa: Ioa) -> Option<DeadbandPolicy> {
        self.policies.remove(&ioa)
    }

    pub fn forget(&mut self, ioa: Ioa) {
        self.baselines.remove(&ioa);
    }

    pub fn clear(&mut self) {
        self.baselines.clear();
    }

    pub fn evaluate(
        &mut self,
        ioa: Ioa,
        value: MonitoredValue,
        quality: Qds,
    ) -> Result<EmitDecision, DeadbandError> {
        // Rule 1: no baseline → first sample.
        let Some(baseline) = self.baselines.get(&ioa).copied() else {
            self.baselines.insert(ioa, Baseline { value, quality });
            return Ok(EmitDecision::Emit);
        };

        // Rule 3 (checked before quality so we don't accidentally "emit" a
        // wrong-kind sample): kind mismatch is an error; baseline untouched.
        if baseline.value.kind() != value.kind() {
            return Err(DeadbandError::KindMismatch {
                ioa,
                expected: baseline.value.kind(),
                actual: value.kind(),
            });
        }

        // Rule 2: quality bits differ → always emit.
        if baseline.quality != quality {
            self.baselines.insert(ioa, Baseline { value, quality });
            return Ok(EmitDecision::Emit);
        }

        let policy = self.policy(ioa);

        // Rule 4: no threshold configured → emit only if value actually changed.
        if matches!(policy, DeadbandPolicy::None) {
            if baseline.value == value {
                return Ok(EmitDecision::Suppress);
            }
            self.baselines.insert(ioa, Baseline { value, quality });
            return Ok(EmitDecision::Emit);
        }

        // Rule 5: threshold comparator — implemented in Task 4.
        let _ = policy;
        unimplemented!("threshold comparator, Task 4")
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

    #[test]
    fn policy_defaults_to_none() {
        let t = DeadbandTracker::new();
        assert_eq!(t.policy(Ioa(99)), DeadbandPolicy::None);
    }

    #[test]
    fn set_policy_and_read_back() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.5 });
        assert_eq!(
            t.policy(Ioa(1)),
            DeadbandPolicy::Absolute { delta: 0.5 }
        );
    }

    #[test]
    fn remove_policy_returns_previous() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.5 });
        let prev = t.remove_policy(Ioa(1));
        assert_eq!(prev, Some(DeadbandPolicy::Absolute { delta: 0.5 }));
        assert_eq!(t.policy(Ioa(1)), DeadbandPolicy::None);
    }

    #[test]
    fn forget_drops_only_named_baseline() {
        let mut t = DeadbandTracker::new();
        let _ = t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        let _ = t.evaluate(Ioa(2), MonitoredValue::Float(2.0), qds()).unwrap();
        t.forget(Ioa(1));
        // IOA 1's next evaluate is first-sample again.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn clear_drops_all_baselines_but_keeps_policies() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.5 });
        let _ = t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        t.clear();
        assert_eq!(
            t.policy(Ioa(1)),
            DeadbandPolicy::Absolute { delta: 0.5 }
        );
        // Baseline gone — first-sample again.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn quality_change_forces_emit_despite_threshold() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 999.0 });
        // Seed baseline.
        t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        // Same value, flip `invalid`.
        let bad = Qds {
            overflow: false,
            quality: Quality { invalid: true, ..Default::default() },
        };
        let decision = t.evaluate(Ioa(1), MonitoredValue::Float(1.0), bad).unwrap();
        assert_eq!(decision, EmitDecision::Emit);
    }

    #[test]
    fn overflow_bit_change_forces_emit() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 999.0 });
        t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        let ovf = Qds { overflow: true, quality: Quality::default() };
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(1.0), ovf).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn kind_mismatch_returns_error_and_keeps_baseline() {
        let mut t = DeadbandTracker::new();
        t.evaluate(Ioa(1), MonitoredValue::Scaled(10), qds()).unwrap();
        let err = t.evaluate(Ioa(1), MonitoredValue::Float(10.0), qds()).unwrap_err();
        assert_eq!(
            err,
            DeadbandError::KindMismatch {
                ioa: Ioa(1),
                expected: ValueKind::Scaled,
                actual: ValueKind::Float,
            }
        );
        // Baseline still Scaled.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Scaled(10), qds()).unwrap(),
            EmitDecision::Suppress
        );
    }

    #[test]
    fn policy_none_always_emits_on_change() {
        let mut t = DeadbandTracker::new();
        t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(1.0001), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn policy_none_same_value_suppresses() {
        let mut t = DeadbandTracker::new();
        t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        // Exactly equal value + equal quality → no change at all → Suppress.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap(),
            EmitDecision::Suppress
        );
    }
}
