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

    /// For strictly monotonic step sequences (cur only grows), a larger
    /// delta produces no more emits than a smaller one. We restrict the
    /// input domain to non-negative steps because deadband emit counts
    /// are NOT globally monotone in delta over arbitrary walks: deferring
    /// an emit anchors the baseline at a stale value, and a later swing
    /// past it can produce more emits than a smaller delta would have.
    /// (The canonical counterexample is steps [0, 0, 0, +5.7, +3.7, −6.7]
    /// at delta=4.05 vs 5.97 → 1 emit vs 2 emits.) The monotonic-walk
    /// restriction keeps the property meaningful and testable.
    #[test]
    fn larger_delta_emits_no_more_often_for_monotonic_walks(
        seed in -100.0_f32..100.0,
        steps in proptest::collection::vec(0.0_f32..10.0, 0..40),
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
            "delta {} → {} emits; delta {} → {} emits (must not grow on monotonic walk)",
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
