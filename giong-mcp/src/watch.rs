//! Samples every worker's pulse ten times a second, to tell a blocked
//! worker from a busy one and to compute rates over the last second.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::observe::{Location, Pulse, WorkerState};

use crate::tools::State;

pub(crate) const INTERVAL: Duration = Duration::from_millis(100);
/// Samples kept per worker: a little over a second.
const KEEP: usize = 12;

#[derive(Default)]
pub(crate) struct Watch {
    workers: Vec<Worker>,
}

#[derive(Default)]
struct Worker {
    samples: VecDeque<(Instant, Pulse)>,
    /// When `ticks` or `polls` last changed, or the worker was last seen
    /// asleep or stopped.
    moved: Option<Instant>,
}

/// A worker that is awake but has not moved on since `since`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Blocked {
    pub(crate) since: Instant,
    /// The task it is in, `None` if it is blocked outside any task.
    pub(crate) task: Option<Location>,
}

/// Rates over the last second.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rates {
    pub(crate) window: Duration,
    pub(crate) polls_per_s: f64,
    pub(crate) ticks_per_s: f64,
    pub(crate) spawned_per_s: f64,
    /// Share of the window spent awake, 0 to 1.
    pub(crate) busy: f64,
}

impl Watch {
    pub(crate) fn sample(&mut self, pulses: Vec<Pulse>, now: Instant) {
        self.workers.resize_with(pulses.len(), Worker::default);
        for (w, pulse) in self.workers.iter_mut().zip(pulses) {
            let moved = match w.samples.back() {
                None => true,
                Some((_, last)) => {
                    pulse.state != WorkerState::Running
                        || pulse.ticks != last.ticks
                        || pulse.polls != last.polls
                }
            };
            if moved || w.moved.is_none() {
                w.moved = Some(now);
            }
            w.samples.push_back((now, pulse));
            while w.samples.len() > KEEP {
                w.samples.pop_front();
            }
        }
    }

    /// Whether worker `i` has been awake without moving on for at least one
    /// sampling interval.
    pub(crate) fn blocked(&self, i: usize, now: Instant) -> Option<Blocked> {
        let w = self.workers.get(i)?;
        let (_, last) = w.samples.back()?;
        let since = w.moved?;
        if last.state != WorkerState::Running || now.duration_since(since) < INTERVAL {
            return None;
        }
        Some(Blocked {
            since,
            task: last.polling,
        })
    }

    pub(crate) fn rates(&self, i: usize) -> Option<Rates> {
        let w = self.workers.get(i)?;
        let (t0, first) = w.samples.front()?;
        let (t1, last) = w.samples.back()?;
        let window = t1.duration_since(*t0);
        if window.is_zero() {
            return None;
        }
        let secs = window.as_secs_f64();
        let per_s = |a: u64, b: u64| b.wrapping_sub(a) as f64 / secs;
        let idle = last.idle.saturating_sub(first.idle).as_secs_f64();
        Some(Rates {
            window,
            polls_per_s: per_s(first.polls, last.polls),
            ticks_per_s: per_s(first.ticks, last.ticks),
            spawned_per_s: per_s(first.spawned, last.spawned),
            busy: (1.0 - idle / secs).clamp(0.0, 1.0),
        })
    }
}

pub(crate) async fn run(state: Rc<State>) {
    loop {
        state.sample_now();
        giong::time::sleep(INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pulse(state: WorkerState, ticks: u64, polls: u64, idle_ms: u64) -> Pulse {
        // Pulse is non-exhaustive: start from a real one.
        let mut p = giong::block_on(async { giong::handle().pulse(0) });
        p.state = state;
        p.ticks = ticks;
        p.polls = polls;
        p.idle = Duration::from_millis(idle_ms);
        p.polling = None;
        p
    }

    #[test]
    fn a_worker_that_stops_moving_is_blocked() {
        let mut watch = Watch::default();
        let t = Instant::now();
        watch.sample(vec![pulse(WorkerState::Running, 1, 1, 0)], t);
        watch.sample(vec![pulse(WorkerState::Running, 2, 5, 0)], t + INTERVAL);
        assert!(watch.blocked(0, t + INTERVAL).is_none());
        watch.sample(vec![pulse(WorkerState::Running, 2, 5, 0)], t + 2 * INTERVAL);
        watch.sample(vec![pulse(WorkerState::Running, 2, 5, 0)], t + 5 * INTERVAL);
        let blocked = watch.blocked(0, t + 5 * INTERVAL).expect("blocked");
        assert_eq!(blocked.since, t + INTERVAL);
        // Asleep is not blocked.
        watch.sample(vec![pulse(WorkerState::Parked, 2, 5, 10)], t + 6 * INTERVAL);
        assert!(watch.blocked(0, t + 6 * INTERVAL).is_none());
    }

    #[test]
    fn rates_come_from_the_oldest_and_newest_samples() {
        let mut watch = Watch::default();
        let t = Instant::now();
        watch.sample(vec![pulse(WorkerState::Running, 0, 0, 0)], t);
        watch.sample(
            vec![pulse(WorkerState::Parked, 100, 1000, 250)],
            t + Duration::from_secs(1),
        );
        let r = watch.rates(0).unwrap();
        assert!((r.polls_per_s - 1000.0).abs() < 1e-6);
        assert!((r.ticks_per_s - 100.0).abs() < 1e-6);
        assert!((r.busy - 0.75).abs() < 1e-6);
    }
}
