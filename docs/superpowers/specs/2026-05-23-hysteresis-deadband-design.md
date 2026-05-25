# Hysteresis / deadband for spontaneous emissions

**Status:** Design, 2026-05-23
**Affected crates:** `iec60870` (new public API), `iec60870-test-tools` (consumer)

## Problem

The test-tools outstation (`iec-server`) emits a `Cause::SPONTANEOUS` ASDU
on every simulator tick and every explicit `Set`. For real-world telemetry
this is far too chatty: an analog sensor drifting by 0.01% on every sample
should not be reported to the master. Real outstations gate spontaneous
emission behind a per-point deadband (hysteresis): the new value must
differ from the last *emitted* value by at least a configured threshold,
either absolute (engineering units) or relative (percentage of last
emitted value).

There is no such facility today in either the core `iec60870` crate or in
the test-tools server. This document specifies one.

## Goals

- Provide a reusable, sans-I/O deadband evaluator in the core crate that
  third-party outstation implementations can drop in without committing to
  a full process-image abstraction.
- Wire it into the test-tools server so spontaneous traffic from the
  simulator becomes representative of real telemetry behavior.
- Preserve current behavior by default: an IOA without a configured
  policy emits on every change.
- Make the "what counts as a change" rules explicit, testable, and
  uniform across all monitored point kinds.

## Non-goals

- Persisting deadband baselines across daemon restarts.
- Cyclic / time-based emission ("send every N seconds even if unchanged").
  Distinct concept; can be added later without touching this design.
- Integral deadband (sum-of-changes between transmissions). Possible
  future extension; out of scope here.
- A full "outstation framework" type that owns the process image. The
  test-tools daemon keeps its `ProcessImage`; the new tracker is a
  collaborator, not a replacement.

## Architecture overview

A new module `crates/iec60870/src/deadband.rs` introduces two public
types and one error type:

- `DeadbandPolicy` — per-IOA configuration (None / Absolute / Percent).
- `DeadbandTracker` — stateful per-IOA store of policies and last-emitted
  baselines; exposes `evaluate` (decide emit/suppress for a candidate
  spontaneous update) and `observe` (refresh baseline after any outgoing
  ASDU carrying the value).
- `DeadbandError::KindMismatch` — surfaced when the kind of an
  observation does not match the kind of the existing baseline.

The module depends only on `iec60870-proto` types (`Ioa`, `Quality`,
`DoublePoint`). No `tokio`, no I/O. It is re-exported from `lib.rs`.

`iec60870-test-tools` consumes the tracker from three sites in
`bin/server.rs`:

1. The simulator tick loop (gates spontaneous emission via `evaluate`).
2. `handle_set` (calls `observe` after emitting).
3. The General Interrogation responder (calls `observe` per IOA).

Control-plane requests `SetDeadband` / `GetDeadband` plus matching
`iec-server deadband set/get` subcommands let test specs configure and
inspect per-point policies at runtime.

## Public API (core crate)

### Types

```rust
/// What changes count as a "real" change for this point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeadbandPolicy {
    /// No deadband — every observation emits.
    None,

    /// Emit when |new - last_emitted| >= delta. For Single/Double, any
    /// transition counts (delta is ignored).
    Absolute { delta: f64 },

    /// Emit when |new - last_emitted| >= (pct/100) * max(|last|, floor).
    /// `floor` prevents divide-by-zero degeneracy near zero. For
    /// Single/Double, any transition counts (pct/floor ignored).
    Percent { pct: f32, floor: f64 },
}

/// Kind-agnostic value carried by every monitored type. Used as input to
/// the tracker; the same enum works for plain Siq/Diq points and for
/// measured types whose quality includes overflow.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MonitoredValue {
    Single(bool),
    Double(DoublePoint),
    Normalized(f32),     // -1.0..=1.0
    Scaled(i16),
    Float(f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind { Single, Double, Normalized, Scaled, Float }

impl MonitoredValue {
    pub fn kind(self) -> ValueKind;
    /// |value| as f64; 0.0 for Single/Double.
    fn magnitude(self) -> f64;
}

/// Outcome of `evaluate`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EmitDecision {
    /// Threshold crossed OR quality differs OR first-ever sample. The
    /// tracker's baseline has been updated to the new snapshot.
    Emit,
    /// Within deadband AND quality unchanged. Baseline untouched.
    Suppress,
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum DeadbandError {
    #[error("IOA {ioa} baseline is {expected:?} but observation is {actual:?}")]
    KindMismatch {
        ioa: Ioa,
        expected: ValueKind,
        actual: ValueKind,
    },
}
```

Threshold parameters are required, not defaulted: callers activating a
policy must supply both `delta` for Absolute or both `pct` and `floor`
for Percent. Activating thresholds is a deliberate act with deliberate
numbers; there are no convenience constructors.

### Tracker

```rust
pub struct DeadbandTracker { /* policies: HashMap<Ioa, DeadbandPolicy>,
                                 baselines: HashMap<Ioa, Baseline> */ }

impl DeadbandTracker {
    pub fn new() -> Self;

    /// Register or replace the policy for an IOA. Default for
    /// unregistered IOAs is `DeadbandPolicy::None` (every observation
    /// emits). Does not interact with the baseline.
    pub fn set_policy(&mut self, ioa: Ioa, policy: DeadbandPolicy);
    pub fn policy(&self, ioa: Ioa) -> DeadbandPolicy;
    pub fn remove_policy(&mut self, ioa: Ioa) -> Option<DeadbandPolicy>;

    /// Record that an ASDU carrying this value+quality was emitted by
    /// the caller (regardless of COT). Resets the baseline. Use from GI
    /// responders, explicit Set handlers, and anywhere else a
    /// non-deadband-gated ASDU goes out.
    pub fn observe(
        &mut self,
        ioa: Ioa,
        value: MonitoredValue,
        quality: Qds,
    ) -> Result<(), DeadbandError>;

    /// Decide whether to emit a candidate spontaneous update. On
    /// `Emit`, the baseline is refreshed; on `Suppress`, it is not. The
    /// caller is responsible for actually sending the ASDU after this
    /// returns `Emit`.
    pub fn evaluate(
        &mut self,
        ioa: Ioa,
        value: MonitoredValue,
        quality: Qds,
    ) -> Result<EmitDecision, DeadbandError>;

    /// Drop the baseline for an IOA so the next `evaluate` emits
    /// unconditionally as a first sample. Policy is unaffected.
    pub fn forget(&mut self, ioa: Ioa);

    /// Drop all baselines (e.g. on a peer reconnect that needs to
    /// re-synchronize). Policies are unaffected.
    pub fn clear(&mut self);
}
```

`Qds` is `iec60870_proto::asdu::ie::Qds`, which already bundles the
four-bit `Quality` set (`blocked / substituted / not_topical / invalid`)
with the `overflow` bit used by measured types. The tracker treats it as
an opaque five-bit bitset and compares it bit-for-bit. For Single/Double
points whose wire quality is plain `Quality` (no overflow), the caller
wraps it as `Qds { overflow: false, quality }`.

### Decision rules (inside `evaluate`)

In order:

1. **No baseline yet** → `Emit`, store new baseline.
2. **Value kind ≠ baseline kind** → `Err(KindMismatch)`, baseline
   untouched. (`observe` follows the same rule.) Checked *before* quality
   so that a quality-flip coincident with a wrong-kind value doesn't
   accidentally store a mismatched baseline.
3. **Quality bits differ from baseline quality** → `Emit`, store new baseline.
4. **Policy is `None`** → emit only if value or quality differs from
   baseline (already covered by rules 1–3; same-value same-quality →
   `Suppress`).
5. **Policy is `Absolute` / `Percent`** → kind-specific comparator.

### Kind-specific comparators

For Single and Double, any transition emits; threshold parameters are
ignored. For Normalized/Scaled/Float:

```rust
let dist = match (baseline.value, new) {
    (Normalized(a), Normalized(b)) => (b - a).abs() as f64,
    (Scaled(a),     Scaled(b))     => (i32::from(b) - i32::from(a)).unsigned_abs() as f64,
    (Float(a),      Float(b))      => (b - a).abs() as f64,
    _ => unreachable!(),
};

let crosses = match policy {
    DeadbandPolicy::Absolute { delta }      => dist >= delta,
    DeadbandPolicy::Percent  { pct, floor } => {
        let ref_mag = baseline.value.magnitude().max(floor);
        dist >= (pct as f64 / 100.0) * ref_mag
    }
    DeadbandPolicy::None => true,
};
```

Notes:

- `Scaled` uses widened integer math to avoid `i16::MIN - i16::MAX`
  overflow; comparison happens in `f64`.
- NaN handling for `Float`: if `b.is_nan() != a.is_nan()`, or either is
  non-finite where the other is finite, treat as a transition and emit.
  Encoded as an explicit guard before the arithmetic comparator.

## Integration: `iec60870-test-tools/bin/server.rs`

Place a `DeadbandTracker` inside `DaemonState` next to `image`, guarded
by the same `RwLock`. Three integration points:

1. **Simulator tick loop** (current location ~lines 974–1004 in
   `bin/server.rs`):
   ```text
   advance value in image
   build MonitoredValue + Quality from updated entry
   match tracker.evaluate(ioa, value, quality):
     Ok(Emit)     → encode + broadcast SPONTANEOUS ASDU
     Ok(Suppress) → no ASDU; still send the SimTick event so observers
                    can see ticks even when traffic is gated
     Err(e)       → tracing::error!(?e); continue
   ```

2. **`handle_set`** (current location ~line 277). Stays unchanged from
   the caller's perspective: the explicit Set always emits SPONTANEOUS.
   After encoding, call `tracker.observe(ioa, value, quality)` so the
   baseline tracks reality and a subsequent simulator tick does not
   immediately re-emit.

3. **General Interrogation responder** (current location ~line 393, the
   per-IOA loop emitting `INTERROGATED_*` COTs). Call
   `tracker.observe(ioa, value, quality)` for each IOA after encoding
   its ASDU.

### Control-plane additions

New `Request` variants in `crates/iec60870-test-tools/src/wire.rs`:

```rust
SetDeadband {
    ioa: u32,
    policy: DeadbandPolicyWire,
},
GetDeadband {
    ioa: u32,
},
```

`DeadbandPolicyWire` is a JSON-tagged enum mirroring the public type:

```jsonc
{ "kind": "none" }
{ "kind": "absolute", "delta": 0.5 }
{ "kind": "percent",  "pct": 5.0, "floor": 0.01 }
```

Corresponding `iec-server deadband set --ioa <N> --policy <json>` and
`iec-server deadband get --ioa <N>` CLI subcommands.

## Edge cases

- **First sample after `forget` or after policy change.** First-sample
  rule (no baseline) → `Emit`. Policy changes do not invalidate the
  baseline; only `forget`/`clear` do.
- **Peer reconnect.** Baselines persist by default. A reconnecting peer
  sees the next sample only if it drifts past threshold. If the caller
  wants every-sample-after-reconnect semantics, it calls `clear()` from
  its connection-closed hook. Not done by default.
- **Quality flips with same value.** Always emits (rule 2). The overflow
  bit is one of the compared bits.
- **NaN / non-finite transitions.** Treated as a change; emit.
- **Kind mismatch.** Surfaced as `Err(KindMismatch)`; baseline untouched
  so the caller can correct configuration and retry.
- **Daemon restart.** Baselines do not persist. The first post-restart
  sample for each IOA emits as a first sample.

## Testing strategy

### Unit tests (`crates/iec60870/src/deadband.rs`)

Sans-I/O, no async, all in one file alongside the implementation:

| Test | Asserts |
|---|---|
| `first_sample_emits` | No baseline → `Emit`, baseline stored. |
| `quality_change_forces_emit` | Same value, different quality bit → `Emit`, even with `Absolute { delta: 999.0 }`. |
| `policy_none_always_emits` | Default (unregistered) → `Emit` on every call. |
| `absolute_below_threshold_suppresses` | Float, baseline 100.0, new 100.4, delta 0.5 → `Suppress`, baseline unchanged. |
| `absolute_at_threshold_emits` | Same setup, new 100.5 → `Emit`, baseline 100.5. |
| `percent_evaluates_against_last_emitted` | Float, baseline 100.0, pct 5%, floor 0.001: 104.9 → `Suppress`; 105.0 → `Emit`. Then baseline=105.0: 110.2 → `Suppress`; 110.25 → `Emit`. |
| `percent_floor_kicks_in_near_zero` | Float, baseline 0.0, pct 5%, floor 1.0 → threshold 0.05: 0.04 → `Suppress`; 0.05 → `Emit`. |
| `single_transition_always_emits_regardless_of_policy` | Single(false) → Single(true) with `Absolute { delta: 999.0 }` → `Emit`. |
| `double_no_transition_suppresses` | Same `DoublePoint::On` with `Absolute { delta: 0.0 }` → `Suppress`. |
| `scaled_integer_math_no_overflow` | i16::MIN to i16::MAX distance equals 65535 in f64. |
| `nan_finite_transition_emits` | Float, baseline 1.0, new NaN → `Emit`. Then NaN → 1.0 → `Emit`. |
| `kind_mismatch_returns_error_and_keeps_baseline` | `evaluate(Float)` after baseline `Scaled` → `Err(KindMismatch)`, baseline still Scaled. |
| `observe_refreshes_baseline_for_subsequent_evaluate` | After `observe(value=105.0)`, `evaluate(value=105.4)` with delta 0.5 → `Suppress`. |
| `forget_then_evaluate_re_emits` | After `forget(ioa)`, next call → `Emit` (first sample). |
| `clear_drops_all_baselines_but_keeps_policies` | After `clear()`, next `evaluate` is first-sample; `policy()` still returns the registered policy. |

### Property tests (proptest)

The codebase already uses `proptest`. New tests:

- **Threshold respect.** For any sequence of `Float` observations and a
  fixed `Absolute { delta }`, every `Emit` distance from the
  baseline-just-before to the new value is ≥ `delta`.
- **Monotonicity in delta.** For a fixed input sequence, increasing
  `delta` produces an emit count ≤ the count for a smaller `delta`.
- **Quality independence.** A sequence that produces only `Suppress` for
  the value channel produces exactly one `Emit` at the index where a
  quality bit is flipped.

### Integration / e2e tests

Under `crates/iec60870-test-tools/tests/` and `tests/specs/`:

- `test_NN_deadband_suppresses_small_drift.sh` — configure a Float
  point with `RandomWalk { step: 0.01 }` and
  `Percent { pct: 50.0, floor: 1.0 }`. Run for N ticks. Assert
  spontaneous-ASDU count is meaningfully lower than tick count.
- `test_NN_deadband_gi_still_reports_latest.sh` — same setup; trigger
  GI; assert GI reports the current image value even though spontaneous
  was being suppressed.
- `test_NN_deadband_set_still_emits.sh` — configure a strict
  `Absolute { delta: 1e9 }`; issue an explicit `Set`; assert a
  SPONTANEOUS ASDU arrives (the explicit-Set path is not gated).

## Implementation order

1. New module `crates/iec60870/src/deadband.rs` with types, tracker,
   unit tests. Re-export from `lib.rs`.
2. Wire `DeadbandTracker` into `DaemonState` in
   `crates/iec60870-test-tools/src/bin/server.rs`. Modify the three
   integration points listed above.
3. Add `Request::SetDeadband` / `Request::GetDeadband` to `wire.rs`,
   matching handler arms in `ServerHandler`, and `iec-server deadband
   set/get` CLI subcommands.
4. Add proptest cases.
5. Add the three e2e shell specs.

Each step builds and tests independently.

## Out-of-scope future work

- Cyclic re-emission (send-every-N-seconds-if-unchanged).
- Integral deadband (accumulate suppressed deltas; emit when the sum
  crosses threshold).
- Per-quality-bit policies (e.g. "only `invalid` forces emit, others
  don't").
- Persisting baselines across daemon restarts.
- Exposing deadband config in the core `Server104` builder. The
  tracker is a library primitive; how an outstation organizes its
  process image is up to the consumer.
