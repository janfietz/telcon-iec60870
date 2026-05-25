# Hysteresis / Deadband Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a per-IOA hysteresis/deadband evaluator to the core `iec60870` crate and wire it into the `iec-server` test daemon so spontaneous emissions are gated by an Absolute or Percent threshold (or unconfigured = always emit).

**Architecture:** New sans-I/O `DeadbandTracker` module in `crates/iec60870/src/deadband.rs` exposing `DeadbandPolicy`, `MonitoredValue`, `EmitDecision`, `DeadbandError`. The test-tools server holds one tracker in `DaemonState`; `evaluate()` gates the simulator tick loop, `observe()` is called on every other outbound path (`handle_set`, GI responder) so baselines reflect what the master last saw. JSON control-plane requests `SetDeadband` / `GetDeadband` plus matching CLI subcommands let test specs configure policies at runtime.

**Tech Stack:** Rust 2021, `tokio` (in test-tools only), `tracing`, `thiserror`, `proptest` (new dev-dep on `iec60870`), `serde`/`serde_json` (test-tools control wire), `bash` + `jq` for e2e specs.

**Spec reference:** [`docs/superpowers/specs/2026-05-23-hysteresis-deadband-design.md`](../specs/2026-05-23-hysteresis-deadband-design.md)

---

## File Map

**Created:**
- `crates/iec60870/src/deadband.rs` — types + tracker + unit tests (one file; ~400 lines).
- `crates/iec60870/tests/deadband_props.rs` — proptest property checks.
- `crates/iec60870-test-tools/tests/specs/test_10_deadband_suppresses_small_drift.sh`
- `crates/iec60870-test-tools/tests/specs/test_11_deadband_gi_reports_latest.sh`
- `crates/iec60870-test-tools/tests/specs/test_12_deadband_set_still_emits.sh`

**Modified:**
- `crates/iec60870/Cargo.toml` — add `proptest` to `dev-dependencies`.
- `crates/iec60870/src/lib.rs` — `mod deadband;` + `pub use deadband::...`.
- `crates/iec60870-test-tools/src/wire.rs` — `DeadbandPolicyWire`, `Request::SetDeadband`, `Request::GetDeadband`.
- `crates/iec60870-test-tools/src/bin/server.rs` — add tracker to `DaemonState`, gate simulator tick, observe in `handle_set` + GI, dispatch new requests, add `deadband set/get` CLI subcommands.

---

## Task 1: Module skeleton + types + first failing test

**Files:**
- Create: `crates/iec60870/src/deadband.rs`
- Modify: `crates/iec60870/src/lib.rs`

- [ ] **Step 1: Add `proptest` to dev-dependencies**

Modify `crates/iec60870/Cargo.toml` `[dev-dependencies]` section, append:

```toml
proptest = { workspace = true }
```

(The workspace already declares `proptest = "1"` so no further wiring needed.)

- [ ] **Step 2: Create `crates/iec60870/src/deadband.rs` with type definitions and the first failing test**

```rust
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
```

- [ ] **Step 3: Wire the module into `lib.rs`**

Modify `crates/iec60870/src/lib.rs`. Add `mod deadband;` next to the other `mod` declarations (alphabetically near `mod driver;`). Add to the public re-export block:

```rust
pub use deadband::{
    DeadbandError, DeadbandPolicy, DeadbandTracker, EmitDecision, MonitoredValue, ValueKind,
};
```

- [ ] **Step 4: Run the failing test**

```bash
cargo test -p iec60870 --lib deadband::tests::first_sample_emits
```

Expected: FAIL — `no method named 'evaluate' found for struct 'DeadbandTracker'`.

- [ ] **Step 5: Commit**

```bash
git add crates/iec60870/src/deadband.rs crates/iec60870/src/lib.rs crates/iec60870/Cargo.toml
git commit -m "feat(deadband): scaffold types and first failing test"
```

---

## Task 2: Implement `set_policy` / `policy` / `remove_policy` / `forget` / `clear`

**Files:**
- Modify: `crates/iec60870/src/deadband.rs`

- [ ] **Step 1: Add failing tests**

Append to the `tests` mod:

```rust
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
```

- [ ] **Step 2: Add the methods to `impl DeadbandTracker`**

Replace the existing `impl DeadbandTracker` block with:

```rust
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
        // No baseline yet — first sample.
        if !self.baselines.contains_key(&ioa) {
            self.baselines.insert(ioa, Baseline { value, quality });
            return Ok(EmitDecision::Emit);
        }
        // Placeholder until Tasks 3-4 fill it in.
        unimplemented!("evaluate body, Tasks 3-4")
    }
}
```

- [ ] **Step 3: Run the new tests**

```bash
cargo test -p iec60870 --lib deadband::tests
```

Expected: the five tests in this task PASS; `first_sample_emits` PASSES; any later tests still hit `unimplemented!` — that's fine, none exist yet.

- [ ] **Step 4: Commit**

```bash
git add crates/iec60870/src/deadband.rs
git commit -m "feat(deadband): implement policy table + baseline lifecycle"
```

---

## Task 3: `evaluate` — quality change, kind mismatch, policy=None

**Files:**
- Modify: `crates/iec60870/src/deadband.rs`

- [ ] **Step 1: Add failing tests**

Append to `tests`:

```rust
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
```

(Note: the last test reflects the rule "policy None always emits *on change*"; an unchanged value with unchanged quality has no change to report.)

- [ ] **Step 2: Replace the `evaluate` body**

```rust
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
    unimplemented!("threshold comparator, Task 4")
}
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p iec60870 --lib deadband::tests
```

Expected: all tests in Tasks 1–3 PASS. Task 4 tests don't exist yet, so the `unimplemented!` path is unreached.

- [ ] **Step 4: Commit**

```bash
git add crates/iec60870/src/deadband.rs
git commit -m "feat(deadband): handle first-sample, quality, kind-mismatch, none paths"
```

---

## Task 4: Threshold comparators (Absolute + Percent + NaN handling)

**Files:**
- Modify: `crates/iec60870/src/deadband.rs`

- [ ] **Step 1: Add failing tests**

Append to `tests`:

```rust
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
```

- [ ] **Step 2: Replace the `unimplemented!` branch with the comparator**

Replace the trailing `unimplemented!` line in `evaluate` with this block:

```rust
    // Rule 5: threshold comparator.
    let crosses = threshold_crossed(baseline.value, value, policy);

    if crosses {
        self.baselines.insert(ioa, Baseline { value, quality });
        Ok(EmitDecision::Emit)
    } else {
        Ok(EmitDecision::Suppress)
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
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p iec60870 --lib deadband::tests
```

Expected: all unit tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/iec60870/src/deadband.rs
git commit -m "feat(deadband): threshold comparators for Absolute and Percent"
```

---

## Task 5: `observe` — baseline refresh for non-gated paths

**Files:**
- Modify: `crates/iec60870/src/deadband.rs`

- [ ] **Step 1: Add failing tests**

```rust
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
```

- [ ] **Step 2: Add the `observe` method to `impl DeadbandTracker`**

Add inside the impl block:

```rust
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
```

- [ ] **Step 3: Run tests**

```bash
cargo test -p iec60870 --lib deadband::tests
```

Expected: all unit tests PASS, including the three new ones.

- [ ] **Step 4: Commit**

```bash
git add crates/iec60870/src/deadband.rs
git commit -m "feat(deadband): add observe() for baseline refresh on non-gated paths"
```

---

## Task 6: Property tests

**Files:**
- Create: `crates/iec60870/tests/deadband_props.rs`

- [ ] **Step 1: Write the proptest file**

```rust
//! Property tests for `iec60870::DeadbandTracker`.

use iec60870::{DeadbandPolicy, DeadbandTracker, EmitDecision, MonitoredValue};
use iec60870_proto::asdu::ie::{Qds, Quality};
use iec60870_proto::asdu::Ioa;
use proptest::prelude::*;

fn qds() -> Qds {
    Qds {
        overflow: false,
        quality: Quality::default(),
    }
}

proptest! {
    /// Every Emit decision implies the absolute distance from the
    /// just-prior baseline to the new value is at least `delta`.
    #[test]
    fn absolute_threshold_respected(
        seed in -1000.0_f32..1000.0,
        steps in proptest::collection::vec(-50.0_f32..50.0, 0..50),
        delta in 0.01_f64..50.0,
    ) {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta });
        // Seed baseline (first-sample emit).
        t.evaluate(Ioa(1), MonitoredValue::Float(seed), qds()).unwrap();
        let mut baseline = f64::from(seed);
        let mut cur = seed;
        for s in steps {
            cur += s;
            let dist_from_baseline = (f64::from(cur) - baseline).abs();
            let dec = t.evaluate(Ioa(1), MonitoredValue::Float(cur), qds()).unwrap();
            if dec == EmitDecision::Emit {
                prop_assert!(dist_from_baseline >= delta - 1e-9,
                    "Emit but dist {} < delta {}", dist_from_baseline, delta);
                baseline = f64::from(cur);
            }
        }
    }

    /// Larger delta never increases the emit count for a fixed input
    /// sequence.
    #[test]
    fn larger_delta_emits_no_more_often(
        seed in -100.0_f32..100.0,
        steps in proptest::collection::vec(-10.0_f32..10.0, 0..40),
        delta_small in 0.01_f64..5.0,
        bump in 0.01_f64..20.0,
    ) {
        let delta_large = delta_small + bump;

        fn count_emits(seed: f32, steps: &[f32], delta: f64) -> usize {
            let mut t = DeadbandTracker::new();
            t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta });
            t.evaluate(Ioa(1), MonitoredValue::Float(seed), qds()).unwrap();
            let mut cur = seed;
            let mut n = 0;
            for s in steps {
                cur += s;
                if let Ok(EmitDecision::Emit) =
                    t.evaluate(Ioa(1), MonitoredValue::Float(cur), qds())
                {
                    n += 1;
                }
            }
            n
        }

        let small = count_emits(seed, &steps, delta_small);
        let large = count_emits(seed, &steps, delta_large);
        prop_assert!(large <= small,
            "delta {} → {} emits; delta {} → {} emits (must not grow)",
            delta_small, small, delta_large, large);
    }

    /// A sequence where only quality changes always emits at the flip.
    #[test]
    fn quality_flip_emits_even_under_huge_threshold(
        seed in -100.0_f32..100.0,
    ) {
        let mut t = DeadbandTracker::new();
        t.set_policy(Ioa(1), DeadbandPolicy::Absolute { delta: 1e9 });
        t.evaluate(Ioa(1), MonitoredValue::Float(seed), qds()).unwrap();
        // Same value, flip invalid bit.
        let flipped = Qds {
            overflow: false,
            quality: Quality { invalid: true, ..Default::default() },
        };
        let dec = t.evaluate(Ioa(1), MonitoredValue::Float(seed), flipped).unwrap();
        prop_assert_eq!(dec, EmitDecision::Emit);
    }
}
```

- [ ] **Step 2: Run the proptests**

```bash
cargo test -p iec60870 --test deadband_props
```

Expected: PASS (proptest runs 256 cases per property by default).

- [ ] **Step 3: Commit**

```bash
git add crates/iec60870/tests/deadband_props.rs
git commit -m "test(deadband): property tests for threshold + monotonicity + quality"
```

---

## Task 7: Wire-format support in `iec60870-test-tools`

**Files:**
- Modify: `crates/iec60870-test-tools/src/wire.rs`

- [ ] **Step 1: Add `DeadbandPolicyWire` + Request variants**

Locate the `Request` enum in `crates/iec60870-test-tools/src/wire.rs` (under the `-- server side --` block). Add a new enum type above it (alongside `SimSchedule`):

```rust
/// Per-IOA deadband policy in JSON wire form. Maps 1:1 to
/// `iec60870::DeadbandPolicy`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeadbandPolicyWire {
    /// No deadband — every observation emits.
    None,
    /// Emit when |new − last| ≥ delta.
    Absolute { delta: f64 },
    /// Emit when |new − last| ≥ (pct/100) * max(|last|, floor).
    Percent { pct: f32, floor: f64 },
}
```

Then add the two server-side request variants inside `Request`, in the
`-- server side --` group (after `SimSet`):

```rust
    /// Set the per-IOA deadband policy.
    SetDeadband {
        ioa: u32,
        policy: DeadbandPolicyWire,
    },
    /// Read the per-IOA deadband policy.
    GetDeadband {
        ioa: u32,
    },
```

- [ ] **Step 2: Add a conversion helper to map wire → library policy**

Append below the `DeadbandPolicyWire` definition:

```rust
impl DeadbandPolicyWire {
    /// Map this wire form to the core-crate `DeadbandPolicy`. Kept here
    /// so the binaries don't need to import both modules just to convert.
    pub fn into_policy(self) -> iec60870::DeadbandPolicy {
        match self {
            DeadbandPolicyWire::None => iec60870::DeadbandPolicy::None,
            DeadbandPolicyWire::Absolute { delta } => {
                iec60870::DeadbandPolicy::Absolute { delta }
            }
            DeadbandPolicyWire::Percent { pct, floor } => {
                iec60870::DeadbandPolicy::Percent { pct, floor }
            }
        }
    }

    /// Inverse of `into_policy`.
    pub fn from_policy(p: iec60870::DeadbandPolicy) -> Self {
        match p {
            iec60870::DeadbandPolicy::None => DeadbandPolicyWire::None,
            iec60870::DeadbandPolicy::Absolute { delta } => {
                DeadbandPolicyWire::Absolute { delta }
            }
            iec60870::DeadbandPolicy::Percent { pct, floor } => {
                DeadbandPolicyWire::Percent { pct, floor }
            }
        }
    }
}
```

- [ ] **Step 3: Compile-check the workspace**

```bash
cargo check -p iec60870-test-tools
```

Expected: clean compile. (`iec60870-test-tools/Cargo.toml` already depends on `iec60870` — confirm with `grep iec60870 crates/iec60870-test-tools/Cargo.toml` before continuing; if missing, add `iec60870 = { path = "../iec60870" }` to its `[dependencies]`.)

- [ ] **Step 4: Commit**

```bash
git add crates/iec60870-test-tools/src/wire.rs
# (and Cargo.toml if you had to add the dependency)
git commit -m "feat(test-tools): wire shapes for SetDeadband/GetDeadband"
```

---

## Task 8: Hold a `DeadbandTracker` in `DaemonState` and gate the simulator tick

**Files:**
- Modify: `crates/iec60870-test-tools/src/bin/server.rs`

- [ ] **Step 1: Add tracker to `DaemonState`**

Find the `struct DaemonState { ... }` definition (~line 144 in `bin/server.rs`). Add a new field:

```rust
    /// Per-IOA deadband state for gating spontaneous emissions.
    tracker: iec60870::DeadbandTracker,
```

In `impl DaemonState::new`, initialize it alongside `image`:

```rust
        Self {
            image,
            tracker: iec60870::DeadbandTracker::new(),
            peers: HashMap::new(),
            outstation_tx: None,
            start_time: Instant::now(),
            transport_kind: kind,
            coa: CommonAddress(coa),
        }
```

- [ ] **Step 2: Add a helper module for `PointEntry → MonitoredValue/Qds`**

Open `crates/iec60870-test-tools/src/points.rs` and append at the end:

```rust
/// Convert this entry's value and quality into the kind-agnostic forms
/// the core `iec60870::DeadbandTracker` consumes.
pub fn entry_to_monitored(entry: &PointEntry) -> (iec60870::MonitoredValue, iec60870_proto::asdu::ie::Qds) {
    use iec60870::MonitoredValue;
    let value = match (entry.kind, &entry.value) {
        (PointKind::SpNa | PointKind::SpTb, PointValue::Single(b)) => MonitoredValue::Single(*b),
        (PointKind::DpNa | PointKind::DpTb, PointValue::Double(dpw)) => {
            MonitoredValue::Double(match dpw {
                DoublePointWire::Intermediate => iec60870_proto::asdu::ie::DoublePoint::Intermediate,
                DoublePointWire::Off => iec60870_proto::asdu::ie::DoublePoint::Off,
                DoublePointWire::On => iec60870_proto::asdu::ie::DoublePoint::On,
                DoublePointWire::Indeterminate => iec60870_proto::asdu::ie::DoublePoint::Indeterminate,
            })
        }
        (PointKind::MeNa | PointKind::MeTd, PointValue::Normalized(f)) => MonitoredValue::Normalized(*f),
        (PointKind::MeNb | PointKind::MeTe, PointValue::Scaled(s)) => MonitoredValue::Scaled(*s),
        (PointKind::MeNc | PointKind::MeTf, PointValue::Float(f)) => MonitoredValue::Float(*f),
        // Fallback for value/kind mismatch in the image — shouldn't happen.
        _ => MonitoredValue::Single(false),
    };
    let qds = qds_from_wire(entry.quality);
    (value, qds)
}
```

- [ ] **Step 3: Gate the simulator tick loop with `evaluate`**

Find the simulator tick block in `bin/server.rs` around lines 960–1005. Replace the body of the per-IOA `tokio::spawn(async move { ... })` from `loop { ticker.tick().await; elapsed = ...;` through the broadcast block with:

```rust
            loop {
                ticker.tick().await;
                elapsed = elapsed.wrapping_add(1);

                let (bytes, kind_str) = {
                    let mut s = state_tick.write().await;
                    let ca = s.coa;
                    let schedule = match s.image.get(ioa) {
                        Some(e) => e.schedule.clone(),
                        None => return,
                    };
                    let current = s.image.get(ioa).map(|e| e.value.clone());
                    if let Some(v) = advance_value(&schedule, current.as_ref(), elapsed) {
                        s.image.set(ioa, v, None);
                    }
                    // Read updated entry + decide via deadband.
                    let (val, qds) = match s.image.get(ioa) {
                        Some(e) => iec60870_test_tools::points::entry_to_monitored(e),
                        None => return,
                    };
                    let decision = match s.tracker.evaluate(Ioa(ioa), val, qds) {
                        Ok(d) => d,
                        Err(e) => {
                            tracing::error!(?e, ioa, "deadband evaluate error");
                            iec60870::EmitDecision::Suppress
                        }
                    };
                    let kind_str = s
                        .image
                        .get(ioa)
                        .map(|e| format!("{:?}", e.kind))
                        .unwrap_or_default();
                    let bytes = if matches!(decision, iec60870::EmitDecision::Emit) {
                        s.image
                            .get(ioa)
                            .and_then(|e| encode_point(ioa, e, Cot::with(Cause::SPONTANEOUS), ca))
                    } else {
                        None
                    };
                    (bytes, kind_str)
                };

                if let Some(bytes) = bytes {
                    let s = state_tick.read().await;
                    s.broadcast(bytes).await;
                }

                // Always emit the SimTick event so observers see ticks even
                // when the value-channel is suppressed by deadband.
                let _ = event_tx_tick.send(Event::SimTick { ioa, kind: kind_str });
            }
```

Add the needed imports near the top of `bin/server.rs` if not already present:

```rust
use iec60870_proto::asdu::Ioa;
```

(`iec60870::EmitDecision` and `iec60870::MonitoredValue` are referenced through fully qualified paths above so you don't have to scrub the imports list.)

- [ ] **Step 4: Build and run existing tests**

```bash
cargo build -p iec60870-test-tools --bins
cargo test -p iec60870 --lib
cargo test -p iec60870 --test deadband_props
```

Expected: clean build; all library + property tests still pass. No new e2e specs yet.

- [ ] **Step 5: Commit**

```bash
git add crates/iec60870-test-tools/src/bin/server.rs crates/iec60870-test-tools/src/points.rs
git commit -m "feat(server): gate simulator tick spontaneous emission through DeadbandTracker"
```

---

## Task 9: `observe` in `handle_set` and the General Interrogation responder

**Files:**
- Modify: `crates/iec60870-test-tools/src/bin/server.rs`

- [ ] **Step 1: Update `handle_set` to refresh the baseline after encoding**

Find `handle_set` (around line 277). Replace the function body with:

```rust
async fn handle_set(
    &self,
    ioa: u32,
    value: PointValue,
    quality: Option<QualityWire>,
) -> Response {
    let bytes = {
        let mut state = self.state.write().await;
        if !state.image.set(ioa, value, quality) {
            return Response::err(format!("IOA {ioa} not found"));
        }
        let entry = state.image.get(ioa).unwrap().clone();
        let coa = state.coa;
        let bytes = encode_point(ioa, &entry, Cot::with(Cause::SPONTANEOUS), coa);
        // Baseline tracks any outgoing ASDU carrying the value.
        let (val, qds) = iec60870_test_tools::points::entry_to_monitored(&entry);
        if let Err(e) = state.tracker.observe(Ioa(ioa), val, qds) {
            tracing::error!(?e, ioa, "deadband observe error in handle_set");
        }
        bytes
    };

    if let Some(bytes) = bytes {
        let state = self.state.read().await;
        state.broadcast(bytes).await;
    }

    Response::ok_empty()
}
```

- [ ] **Step 2: Update the General Interrogation responder**

Find the GI responder around line 393 (search for `INTERROGATED_GENERAL`). Identify the per-IOA encode loop. After each `encode_point(...)` call inside the GI body, add an `observe` call. Locate the existing loop, which roughly reads:

```rust
let mut asdus = Vec::new();
for (ioa, entry) in state.image.iter() {
    if let Some(bytes) = encode_point(ioa, entry, Cot::with(Cause::INTERROGATED_GENERAL), ca) {
        asdus.push(bytes);
    }
}
```

Restructure to also call `observe`. Because the existing handler takes `&self.state` as a read lock for GI, switch the GI handler to take a `write` lock so the tracker can be mutated. Replace the loop with:

```rust
let asdus_to_send: Vec<Vec<u8>> = {
    let mut state = self.state.write().await;
    let ca = state.coa;
    let mut acc = Vec::new();
    // Collect (ioa, monitored, qds) pairs first to avoid the borrow
    // checker conflict between image.iter() and tracker.observe().
    let snapshots: Vec<(u32, iec60870::MonitoredValue, iec60870_proto::asdu::ie::Qds, Vec<u8>)> =
        state
            .image
            .iter()
            .filter_map(|(ioa, entry)| {
                let bytes = encode_point(ioa, entry, Cot::with(Cause::INTERROGATED_GENERAL), ca)?;
                let (val, qds) = iec60870_test_tools::points::entry_to_monitored(entry);
                Some((ioa, val, qds, bytes))
            })
            .collect();
    for (ioa, val, qds, bytes) in snapshots {
        if let Err(e) = state.tracker.observe(Ioa(ioa), val, qds) {
            tracing::error!(?e, ioa, "deadband observe error in GI");
        }
        acc.push(bytes);
    }
    acc
};
```

Then iterate `asdus_to_send` and broadcast as before. The existing surrounding code that wraps with `ACTIVATION_CON` / `ACTIVATION_TERMINATION` envelopes stays unchanged — only the per-IOA loop body changes.

Do the same for the group-interrogation responder (around line 432, `Cause::interrogated_group(group)`).

- [ ] **Step 3: Build and run library + existing specs**

```bash
cargo build -p iec60870-test-tools --bins
cargo test -p iec60870 --lib
cargo test -p iec60870 --test deadband_props
# Run the existing e2e to make sure GI + spontaneous still function:
bash crates/iec60870-test-tools/tests/specs/test_03_interrogation_general.sh
bash crates/iec60870-test-tools/tests/specs/test_06_spontaneous.sh
```

Expected: all pass. Spec 06 still observes spontaneous emissions because unregistered IOAs default to `DeadbandPolicy::None` (always emit on change), and the simulator changes values on every tick.

- [ ] **Step 4: Commit**

```bash
git add crates/iec60870-test-tools/src/bin/server.rs
git commit -m "feat(server): refresh deadband baseline on Set and GI/group responses"
```

---

## Task 10: `SetDeadband` / `GetDeadband` request handlers + CLI subcommands

**Files:**
- Modify: `crates/iec60870-test-tools/src/bin/server.rs`

- [ ] **Step 1: Handle the new requests**

In `ServerHandler::handle` (around line 195), add arms for the two new variants. After the `Request::SimSet` arm:

```rust
Request::SetDeadband { ioa, policy } => self.handle_set_deadband(ioa, policy).await,
Request::GetDeadband { ioa } => self.handle_get_deadband(ioa).await,
```

The `_ =>` / `client-only ops` arm at the bottom remains unchanged.

Add the two handler methods inside `impl ServerHandler`:

```rust
async fn handle_set_deadband(
    &self,
    ioa: u32,
    policy: iec60870_test_tools::wire::DeadbandPolicyWire,
) -> Response {
    let mut state = self.state.write().await;
    state.tracker.set_policy(Ioa(ioa), policy.into_policy());
    Response::ok_empty()
}

async fn handle_get_deadband(&self, ioa: u32) -> Response {
    let state = self.state.read().await;
    let policy = state.tracker.policy(Ioa(ioa));
    let wire = iec60870_test_tools::wire::DeadbandPolicyWire::from_policy(policy);
    let data = serde_json::json!({ "ioa": ioa, "policy": wire });
    Response::ok(data)
}
```

- [ ] **Step 2: Add CLI subcommands**

In `bin/server.rs`, locate the `CliCommand` enum (~line 56). Add a variant:

```rust
    /// Deadband sub-commands.
    Deadband(DeadbandArgs),
```

Then add the new arg structs near the existing `SimArgs`:

```rust
#[derive(Args, Debug)]
struct DeadbandArgs {
    #[command(subcommand)]
    command: DeadbandSubcommand,
}

#[derive(Subcommand, Debug)]
enum DeadbandSubcommand {
    /// Read one IOA's deadband policy.
    Get(DeadbandGetArgs),
    /// Set one IOA's deadband policy.
    Set(DeadbandSetArgs),
}

#[derive(Args, Debug)]
struct DeadbandGetArgs {
    #[arg(long)]
    ioa: u32,
}

#[derive(Args, Debug)]
struct DeadbandSetArgs {
    #[arg(long)]
    ioa: u32,
    /// JSON policy, e.g.:
    ///   `{"kind":"none"}`
    ///   `{"kind":"absolute","delta":0.5}`
    ///   `{"kind":"percent","pct":5.0,"floor":0.001}`
    #[arg(long)]
    policy: String,
}
```

Then wire the dispatch in `main()` (the `match cli.command { ... }` block, near where `CliCommand::Sim(...)` is handled):

```rust
CliCommand::Deadband(args) => match args.command {
    DeadbandSubcommand::Get(g) => {
        client_call(&socket, &Request::GetDeadband { ioa: g.ioa }).await?;
    }
    DeadbandSubcommand::Set(s) => {
        let policy: iec60870_test_tools::wire::DeadbandPolicyWire =
            serde_json::from_str(&s.policy)
                .map_err(|e| anyhow::anyhow!("invalid policy JSON: {e}"))?;
        client_call(&socket, &Request::SetDeadband { ioa: s.ioa, policy }).await?;
    }
},
```

(Mirrors the existing `CliCommand::Sim` wiring — `client_call` is the helper in `bin/server.rs` that POSTs to the control socket via `control::call` and prints the response.)

- [ ] **Step 3: Build and smoke-test against a running daemon**

```bash
cargo build -p iec60870-test-tools --bins
```

Manual smoke test (in two terminals or backgrounded):

```bash
SOCK=/tmp/iec-smoke-$$.sock
./target/debug/iec-server --control "$SOCK" daemon --transport tcp --addr 127.0.0.1:24040 &
sleep 0.5
./target/debug/iec-server --control "$SOCK" deadband get --ioa 300
./target/debug/iec-server --control "$SOCK" deadband set --ioa 300 --policy '{"kind":"absolute","delta":0.5}'
./target/debug/iec-server --control "$SOCK" deadband get --ioa 300
./target/debug/iec-server --control "$SOCK" shutdown
```

Expected: the first `get` returns `{"kind":"none"}`; the post-set `get` returns the absolute policy.

- [ ] **Step 4: Commit**

```bash
git add crates/iec60870-test-tools/src/bin/server.rs
git commit -m "feat(server): SetDeadband/GetDeadband requests + iec-server deadband CLI"
```

---

## Task 11: E2E spec — deadband suppresses small drift

**Files:**
- Create: `crates/iec60870-test-tools/tests/specs/test_10_deadband_suppresses_small_drift.sh`

- [ ] **Step 1: Write the spec script**

```bash
#!/usr/bin/env bash
# Spec 10 — A configured deadband suppresses below-threshold spontaneous
# emissions.
#
# Setup: configure IOA 300 (a normalized random-walk point with step
# 0.05) with `Percent { pct: 200, floor: 1.0 }`. The threshold becomes
# max(|last|, 1.0) * 2.0 = 2.0 in engineering units — vastly larger than
# any single random-walk step of 0.05, so the simulator should suppress
# almost every tick after the first-sample emit.

source "$(dirname "$0")/lib.sh"
setup_test "10_deadband_suppresses_small_drift"

start_server
start_client

# Configure a very large percent threshold so almost every tick is suppressed.
srv deadband set --ioa 300 \
    --policy '{"kind":"percent","pct":200.0,"floor":1.0}'

# Subscribe to client events.
EVENTS_FILE="$WORKDIR/client-events.jsonl"
"$CLIENT_BIN" events --socket "$CSOCK" > "$EVENTS_FILE" 2>&1 &
EV_PID=$!
trap '[[ -n "${EV_PID:-}" ]] && kill "$EV_PID" 2>/dev/null || true; teardown' EXIT

# Collect for ~3 seconds (~3 random-walk ticks at 1000 ms each).
sleep 3.0
kill "$EV_PID" 2>/dev/null || true
sleep 0.1

# Count spontaneous M_ME_NA_1 (TypeID 9) events for IOA 300.
n_spont_300=$(jq -c --argjson tid 9 \
    'select(.event=="asdu_received" and .cot=="3" and .type_id==$tid and .ioa==300)' \
    "$EVENTS_FILE" | wc -l)

# We allow up to 2 emits (first sample + possibly one large-drift outlier).
# Without the deadband we would expect ~3.
[[ "$n_spont_300" -le 2 ]] || \
    fail "deadband ineffective: $n_spont_300 spontaneous emits for IOA 300 (expected ≤ 2)"

pass
```

- [ ] **Step 2: Make it executable and run it**

```bash
chmod +x crates/iec60870-test-tools/tests/specs/test_10_deadband_suppresses_small_drift.sh
cargo build -p iec60870-test-tools --bins
bash crates/iec60870-test-tools/tests/specs/test_10_deadband_suppresses_small_drift.sh
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/iec60870-test-tools/tests/specs/test_10_deadband_suppresses_small_drift.sh
git commit -m "test(deadband): e2e spec — large threshold suppresses small drift"
```

---

## Task 12: E2E spec — GI still reports latest value despite deadband

**Files:**
- Create: `crates/iec60870-test-tools/tests/specs/test_11_deadband_gi_reports_latest.sh`

- [ ] **Step 1: Write the spec script**

```bash
#!/usr/bin/env bash
# Spec 11 — When spontaneous emission is suppressed by a tight deadband,
# General Interrogation still reports the latest image value.
#
# Setup: configure IOA 300 with an enormous absolute threshold so no
# spontaneous emit will ever cross it. The simulator continues to mutate
# the image. A GI must still return whatever the image holds at request
# time.

source "$(dirname "$0")/lib.sh"
setup_test "11_deadband_gi_reports_latest"

start_server
start_client

srv deadband set --ioa 300 \
    --policy '{"kind":"absolute","delta":1000000000.0}'

# Let the simulator tick a few times so the image value drifts away
# from its initial 0.0.
sleep 2.0

# Fire a general interrogation and grab the cached read on the client.
cli interrogate --group 3 --timeout-ms 3000
sleep 0.3
read_resp=$(cli read --ioa 300 --type-id 9)
assert_ok "$read_resp" "client cache holds IOA 300 after GI"

# The value should NOT be NaN/missing; the GI path is independent of the
# deadband so the cache must reflect the simulator's actual current value.
value=$(echo "$read_resp" | jq -r '.data.value.value // "null"')
[[ "$value" != "null" ]] || fail "client cache returned null value for IOA 300 after GI"
[[ "$value" != "0" && "$value" != "0.0" && "$value" != "0.000000" ]] \
    || fail "value still 0 after 2 s of random-walk ticks (drift unlikely zero): $value"

pass
```

- [ ] **Step 2: Make it executable and run it**

```bash
chmod +x crates/iec60870-test-tools/tests/specs/test_11_deadband_gi_reports_latest.sh
bash crates/iec60870-test-tools/tests/specs/test_11_deadband_gi_reports_latest.sh
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/iec60870-test-tools/tests/specs/test_11_deadband_gi_reports_latest.sh
git commit -m "test(deadband): e2e spec — GI reports latest value despite deadband"
```

---

## Task 13: E2E spec — explicit `Set` still emits despite deadband

**Files:**
- Create: `crates/iec60870-test-tools/tests/specs/test_12_deadband_set_still_emits.sh`

- [ ] **Step 1: Write the spec script**

```bash
#!/usr/bin/env bash
# Spec 12 — An explicit Set (server-side) still produces a SPONTANEOUS
# ASDU even when an enormous deadband is configured. The Set path is
# not gated by the deadband (only the simulator tick path is).

source "$(dirname "$0")/lib.sh"
setup_test "12_deadband_set_still_emits"

start_server
start_client

# Disable the simulator on IOA 400 so we control all sources of change.
srv sim set --ioa 400 --schedule '{"kind":"none"}'
# Configure a huge threshold so no spontaneous-from-tick would ever fire.
srv deadband set --ioa 400 \
    --policy '{"kind":"absolute","delta":1000000000.0}'

# Subscribe to client events.
EVENTS_FILE="$WORKDIR/client-events.jsonl"
"$CLIENT_BIN" events --socket "$CSOCK" > "$EVENTS_FILE" 2>&1 &
EV_PID=$!
trap '[[ -n "${EV_PID:-}" ]] && kill "$EV_PID" 2>/dev/null || true; teardown' EXIT

# Trigger an explicit Set on the server side.
srv set --ioa 400 --kind me-nb --value 42

# Give time for the ASDU to traverse and the client to log it.
sleep 0.6
kill "$EV_PID" 2>/dev/null || true
sleep 0.1

# Expect at least one spontaneous M_ME_NB_1 (TypeID 11) event for IOA 400.
n_spont_400=$(jq -c --argjson tid 11 \
    'select(.event=="asdu_received" and .cot=="3" and .type_id==$tid and .ioa==400)' \
    "$EVENTS_FILE" | wc -l)
[[ "$n_spont_400" -ge 1 ]] || \
    fail "Set was suppressed by deadband: no spontaneous event for IOA 400 (n=$n_spont_400)"

pass
```

- [ ] **Step 2: Make it executable and run it**

```bash
chmod +x crates/iec60870-test-tools/tests/specs/test_12_deadband_set_still_emits.sh
bash crates/iec60870-test-tools/tests/specs/test_12_deadband_set_still_emits.sh
```

Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/iec60870-test-tools/tests/specs/test_12_deadband_set_still_emits.sh
git commit -m "test(deadband): e2e spec — explicit Set still emits SPONTANEOUS"
```

---

## Task 14: Full workspace verification

**Files:**
- None modified; verification only.

- [ ] **Step 1: Run the full workspace test suite**

```bash
cargo test --workspace --all-features
```

Expected: PASS.

- [ ] **Step 2: Run every e2e spec**

```bash
cargo build -p iec60870-test-tools --bins
for s in crates/iec60870-test-tools/tests/specs/test_*.sh; do
    bash "$s" || { echo "FAILED: $s"; exit 1; }
done
echo "all e2e specs passed"
```

Expected: every spec PASSES, including the three new ones (10, 11, 12) and all existing ones (01–09).

- [ ] **Step 3: Lint**

```bash
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
```

Expected: no warnings, no formatting drift.

- [ ] **Step 4: Final commit** (only if any fixups were made)

```bash
git add -A
git status   # confirm nothing surprising
# only if there's anything staged:
git commit -m "chore: clippy/fmt fixes after deadband implementation"
```

---

## Spec coverage map

| Spec section | Implementing task(s) |
|---|---|
| Module placement, `lib.rs` re-exports | Task 1 |
| `DeadbandPolicy` (None / Absolute / Percent) | Task 1 |
| `MonitoredValue`, `ValueKind`, `magnitude()` | Task 1 |
| `EmitDecision`, `DeadbandError::KindMismatch` | Task 1, Task 3 |
| `DeadbandTracker` lifecycle methods | Task 2 |
| `evaluate` — first sample, quality, kind mismatch, policy=None | Task 3 |
| `evaluate` — comparators per kind, NaN handling | Task 4 |
| `observe` for baseline refresh | Task 5 |
| Property tests | Task 6 |
| Wire-format additions (`DeadbandPolicyWire`, `Set/GetDeadband`) | Task 7 |
| `DaemonState` integration + simulator tick gating | Task 8 |
| `handle_set` baseline refresh | Task 9 |
| GI + group-interrogation responders baseline refresh | Task 9 |
| Request handlers + CLI subcommands | Task 10 |
| E2E: deadband suppresses small drift | Task 11 |
| E2E: GI still reports latest | Task 12 |
| E2E: explicit Set still emits | Task 13 |
| Full workspace verification | Task 14 |
