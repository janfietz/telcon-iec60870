//! Server-side autonomous simulator.
//!
//! Each scheduled IOA runs on its own tokio interval. On every tick the
//! simulator advances the stored point's value according to its
//! [`SimSchedule`][crate::wire::SimSchedule], then notifies the daemon to
//! emit a spontaneous ASDU carrying the new value and pushes a `SimTick`
//! event on the broadcast channel.

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::{broadcast, Mutex};

use crate::points::ProcessImage;
use crate::wire::{DoublePointWire, Event, PointValue, SimSchedule};

/// A handle the simulator gives back to the daemon so it can stop all tasks.
pub struct SimHandle {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl SimHandle {
    /// Abort all simulator tasks.
    pub fn abort(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl std::fmt::Debug for SimHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimHandle")
            .field("tasks", &self.tasks.len())
            .finish()
    }
}

/// Callback type invoked on each simulator tick: `(ioa, new_value, new_quality)`.
/// The daemon uses this to emit spontaneous ASDUs.
pub type TickCallback = Arc<dyn Fn(u32) + Send + Sync + 'static>;

/// Spawn one tokio task per scheduled IOA.
///
/// * `image` — shared process image (locked per tick, held briefly).
/// * `event_tx` — broadcast channel for `SimTick` events.
/// * `on_tick` — called with the IOA after the value is updated, so the
///   caller can emit a spontaneous ASDU.
pub fn spawn_simulator(
    image: Arc<Mutex<ProcessImage>>,
    event_tx: broadcast::Sender<Event>,
    on_tick: TickCallback,
) -> SimHandle {
    // Collect (ioa, schedule, interval_ms) while we hold the lock once.
    let entries: Vec<(u32, SimSchedule, u64)> = {
        // We can't hold an async lock here in sync context, but spawn_simulator
        // is called from async main before any tasks are hot.  Use try_lock.
        let guard = image.try_lock().expect("image not yet shared");
        guard
            .iter()
            .filter_map(|(ioa, entry)| {
                let interval_ms = schedule_interval_ms(&entry.schedule)?;
                Some((ioa, entry.schedule.clone(), interval_ms))
            })
            .collect()
    };

    let mut tasks = Vec::with_capacity(entries.len());

    for (ioa, schedule, interval_ms) in entries {
        let image = Arc::clone(&image);
        let event_tx = event_tx.clone();
        let on_tick = Arc::clone(&on_tick);
        let phase_ms = schedule_phase_ms(&schedule);

        let task = tokio::spawn(async move {
            // Apply phase offset before the first tick.
            if phase_ms > 0 {
                tokio::time::sleep(Duration::from_millis(phase_ms)).await;
            }
            let mut interval =
                tokio::time::interval(Duration::from_millis(interval_ms));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            let mut elapsed_ticks: u64 = 0;

            loop {
                interval.tick().await;
                elapsed_ticks = elapsed_ticks.wrapping_add(1);

                // Advance value in the image.
                {
                    let mut img = image.lock().await;
                    // Re-read the schedule in case it was updated via SimSet.
                    let schedule = match img.get(ioa) {
                        Some(e) => e.schedule.clone(),
                        None => return,
                    };
                    if let Some(new_value) = advance(&schedule, img.get(ioa).map(|e| &e.value), elapsed_ticks) {
                        img.set(ioa, new_value, None);
                    }
                }

                let kind_str = {
                    let img = image.lock().await;
                    img.get(ioa)
                        .map(|e| format!("{:?}", e.kind))
                        .unwrap_or_default()
                };

                // Notify broadcast subscribers.
                let _ = event_tx.send(Event::SimTick {
                    ioa,
                    kind: kind_str,
                });

                // Call the spontaneous-send callback.
                on_tick(ioa);
            }
        });

        tasks.push(task);
    }

    SimHandle { tasks }
}

/// Extract the primary tick interval from a schedule.
fn schedule_interval_ms(sched: &SimSchedule) -> Option<u64> {
    match sched {
        SimSchedule::None => None,
        SimSchedule::Toggle { interval_ms, .. }
        | SimSchedule::Rotate { interval_ms }
        | SimSchedule::RandomWalk { interval_ms, .. }
        | SimSchedule::StepUp { interval_ms, .. }
        | SimSchedule::Sine { interval_ms, .. } => Some(*interval_ms),
    }
}

/// Extract the initial phase offset (used for staggering).
fn schedule_phase_ms(sched: &SimSchedule) -> u64 {
    match sched {
        SimSchedule::Toggle { phase_ms, .. } => *phase_ms,
        _ => 0,
    }
}

/// Compute the next value for the given schedule given the current value and
/// the number of ticks elapsed (1-based). Returns `None` when no change is
/// needed (e.g. `SimSchedule::None`).
fn advance(
    schedule: &SimSchedule,
    current: Option<&PointValue>,
    elapsed_ticks: u64,
) -> Option<PointValue> {
    match schedule {
        SimSchedule::None => None,

        SimSchedule::Toggle { .. } => {
            let on = match current {
                Some(PointValue::Single(b)) => !b,
                _ => true,
            };
            Some(PointValue::Single(on))
        }

        SimSchedule::Rotate { .. } => {
            let next = match current {
                Some(PointValue::Double(DoublePointWire::Off)) => DoublePointWire::On,
                Some(PointValue::Double(DoublePointWire::On)) => DoublePointWire::Off,
                _ => DoublePointWire::Off,
            };
            Some(PointValue::Double(next))
        }

        SimSchedule::RandomWalk { step, min, max, .. } => {
            let current_f = match current {
                Some(PointValue::Normalized(f)) => *f,
                Some(PointValue::Float(f)) => *f,
                _ => 0.0,
            };
            let delta = {
                let mut rng = rand::thread_rng();
                if rng.gen_bool(0.5) { *step } else { -*step }
            };
            let new_val = (current_f + delta).clamp(*min, *max);
            // Detect whether this is a Normalized or Float point.
            match current {
                Some(PointValue::Normalized(_)) => Some(PointValue::Normalized(new_val)),
                _ => Some(PointValue::Float(new_val)),
            }
        }

        SimSchedule::StepUp { step, wrap_at, .. } => {
            let current_i = match current {
                Some(PointValue::Scaled(s)) => i32::from(*s),
                _ => 0_i32,
            };
            let new_val = (current_i + step).rem_euclid(*wrap_at);
            #[allow(clippy::cast_possible_truncation)]
            Some(PointValue::Scaled(new_val.clamp(i16::MIN as i32, i16::MAX as i32) as i16))
        }

        SimSchedule::Sine {
            period_ms,
            interval_ms,
            amplitude,
            offset,
            ..
        } => {
            // Compute phase in full periods.
            let ticks_per_period = (*period_ms).max(1) / (*interval_ms).max(1);
            let phase = if ticks_per_period == 0 {
                0.0_f64
            } else {
                (elapsed_ticks % ticks_per_period) as f64 / ticks_per_period as f64
            };
            let val = (*amplitude as f64 * (2.0 * std::f64::consts::PI * phase).sin()
                + *offset as f64) as f32;
            // Detect Normalized vs Float.
            match current {
                Some(PointValue::Normalized(_)) => {
                    Some(PointValue::Normalized(val.clamp(-1.0, 1.0)))
                }
                _ => Some(PointValue::Float(val)),
            }
        }
    }
}
