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

    /// Record that an ASDU carrying this value+quality was emitted by the
    /// caller (regardless of COT). Resets the baseline. Use from GI
    /// responders, explicit Set handlers, and anywhere else a
    /// non-deadband-gated ASDU goes out.
    pub fn observe(
        &mut self,
        ioa: Ioa,
        value: MonitoredValue,
        quality: Qds,
    ) -> Result<(), DeadbandError> {
        if let Some(baseline) = self.baselines.get(&ioa) {
            if baseline.value.kind() != value.kind() {
                return Err(DeadbandError::KindMismatch {
                    ioa,
                    expected: baseline.value.kind(),
                    actual: value.kind(),
                });
            }
        }
        self.baselines.insert(ioa, Baseline { value, quality });
        Ok(())
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

        // Rule 2: kind mismatch (checked before quality so we don't accidentally
        // "emit" a wrong-kind sample) — error; baseline untouched.
        if baseline.value.kind() != value.kind() {
            return Err(DeadbandError::KindMismatch {
                ioa,
                expected: baseline.value.kind(),
                actual: value.kind(),
            });
        }

        // Rule 3: quality bits differ → always emit.
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

        // Rule 5: threshold comparator.
        let crosses = threshold_crossed(baseline.value, value, policy);

        if crosses {
            self.baselines.insert(ioa, Baseline { value, quality });
            Ok(EmitDecision::Emit)
        } else {
            Ok(EmitDecision::Suppress)
        }
    }
}

/// Compute whether `new` is far enough from `old` for the given policy.
/// Caller has already established that the kinds match. Single/Double
/// short-circuit any-change semantics here; the threshold is ignored.
fn threshold_crossed(
    old: MonitoredValue,
    new: MonitoredValue,
    policy: DeadbandPolicy,
) -> bool {
    use MonitoredValue::*;
    // Single/Double: any transition.
    match (old, new) {
        (Single(a), Single(b)) => return a != b,
        (Double(a), Double(b)) => return a != b,
        _ => {}
    }

    // Numeric kinds. NaN/non-finite transitions count as changes.
    let (a, b) = match (old, new) {
        (Normalized(a), Normalized(b)) | (Float(a), Float(b)) => (f64::from(a), f64::from(b)),
        (Scaled(a), Scaled(b)) => (f64::from(a), f64::from(b)),
        _ => unreachable!("kinds checked by caller"),
    };

    if a.is_nan() != b.is_nan() {
        return true;
    }
    if a.is_finite() != b.is_finite() {
        return true;
    }
    if a.is_nan() && b.is_nan() {
        // Both NaN — no observable change.
        return false;
    }

    let dist = (b - a).abs();
    match policy {
        DeadbandPolicy::None => dist > 0.0, // unreachable in practice (handled earlier)
        DeadbandPolicy::Absolute { delta } => dist >= delta,
        DeadbandPolicy::Percent { pct, floor } => {
            let ref_mag = old.magnitude().max(floor);
            dist >= (f64::from(pct) / 100.0) * ref_mag
        }
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

    // --- Absolute ---------------------------------------------------------

    #[test]
    fn absolute_below_threshold_suppresses() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.5 });
        t.evaluate(Ioa(1), MonitoredValue::Float(100.0), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(100.4), qds()).unwrap(),
            EmitDecision::Suppress
        );
    }

    #[test]
    fn absolute_at_threshold_emits_and_updates_baseline() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.5 });
        t.evaluate(Ioa(1), MonitoredValue::Float(100.0), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(100.5), qds()).unwrap(),
            EmitDecision::Emit
        );
        // Baseline now 100.5; 100.9 (delta 0.4) suppresses.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(100.9), qds()).unwrap(),
            EmitDecision::Suppress
        );
    }

    // --- Percent ----------------------------------------------------------

    #[test]
    fn percent_evaluates_against_last_emitted() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Percent { pct: 5.0, floor: 0.001 });
        t.evaluate(Ioa(1), MonitoredValue::Float(100.0), qds()).unwrap();
        // 4.9% drift → suppress.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(104.9), qds()).unwrap(),
            EmitDecision::Suppress
        );
        // 5.0% drift → emit.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(105.0), qds()).unwrap(),
            EmitDecision::Emit
        );
        // New baseline 105.0; 110.0 is 4.76% → suppress.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(110.0), qds()).unwrap(),
            EmitDecision::Suppress
        );
        // 110.26 is 5.01% from 105.0 → emit.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(110.26), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn percent_floor_kicks_in_near_zero() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Percent { pct: 5.0, floor: 1.0 });
        t.evaluate(Ioa(1), MonitoredValue::Float(0.0), qds()).unwrap();
        // Threshold = 0.05 * max(0, 1) = 0.05.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(0.04), qds()).unwrap(),
            EmitDecision::Suppress
        );
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(0.05), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    // --- Single / Double short-circuit ------------------------------------

    #[test]
    fn single_transition_always_emits_regardless_of_threshold() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 999.0 });
        t.evaluate(Ioa(1), MonitoredValue::Single(false), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Single(true), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn double_no_transition_suppresses_with_threshold() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.0 });
        t.evaluate(Ioa(1), MonitoredValue::Double(DoublePoint::On), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Double(DoublePoint::On), qds()).unwrap(),
            EmitDecision::Suppress
        );
    }

    // --- Scaled / Normalized ----------------------------------------------

    #[test]
    fn scaled_integer_distance_no_overflow() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 65_535.0 });
        t.evaluate(Ioa(1), MonitoredValue::Scaled(i16::MIN), qds()).unwrap();
        // Distance = 65535 → emits at the threshold.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Scaled(i16::MAX), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn normalized_absolute_threshold() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.1 });
        t.evaluate(Ioa(1), MonitoredValue::Normalized(0.0), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Normalized(0.09), qds()).unwrap(),
            EmitDecision::Suppress
        );
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Normalized(0.1), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    // --- NaN / non-finite -------------------------------------------------

    #[test]
    fn nan_to_finite_transition_emits() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 999.0 });
        t.evaluate(Ioa(1), MonitoredValue::Float(f32::NAN), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn finite_to_nan_transition_emits() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 999.0 });
        t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(f32::NAN), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    #[test]
    fn infinity_transition_emits() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 999.0 });
        t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(f32::INFINITY), qds()).unwrap(),
            EmitDecision::Emit
        );
    }

    // --- observe() --------------------------------------------------------

    #[test]
    fn observe_seeds_baseline_so_later_evaluate_suppresses() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.5 });
        t.observe(Ioa(1), MonitoredValue::Float(100.0), qds()).unwrap();
        // Without observe, evaluate(100.4) would emit as first-sample.
        // With observe, the baseline is already 100.0; 100.4 is below 0.5.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(100.4), qds()).unwrap(),
            EmitDecision::Suppress
        );
    }

    #[test]
    fn observe_refreshes_baseline_for_subsequent_evaluate() {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 0.5 });
        t.evaluate(Ioa(1), MonitoredValue::Float(100.0), qds()).unwrap();
        // GI happens — bumps baseline to 105.0.
        t.observe(Ioa(1), MonitoredValue::Float(105.0), qds()).unwrap();
        // Without the observe, 105.4 would cross delta=0.5 from 100.0.
        assert_eq!(
            t.evaluate(Ioa(1), MonitoredValue::Float(105.4), qds()).unwrap(),
            EmitDecision::Suppress
        );
    }

    #[test]
    fn observe_kind_mismatch_returns_error() {
        let mut t = DeadbandTracker::new();
        t.evaluate(Ioa(1), MonitoredValue::Float(1.0), qds()).unwrap();
        let err = t.observe(Ioa(1), MonitoredValue::Scaled(1), qds()).unwrap_err();
        assert_eq!(
            err,
            DeadbandError::KindMismatch {
                ioa: Ioa(1),
                expected: ValueKind::Float,
                actual: ValueKind::Scaled,
            }
        );
    }
}
