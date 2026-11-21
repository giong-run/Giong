//! The adaptive spin governor: how long a worker busy-polls before it
//! sleeps, tuned per worker from what spinning would have caught.
//!
//! After each sleep the worker knows how long it was idle in all (spin plus
//! sleep). Idle no longer than the maximum spin means the next event came
//! soon after the worker gave up: spinning longer would have caught it, so
//! the window doubles. Idle more than twice the maximum means spinning only
//! burned the CPU: the window halves, down to none. Between the two nothing
//! changes, so a load whose gaps sit near the maximum does not make the
//! window swing (hysteresis). A token bucket caps the share of time spent
//! spinning: tokens accrue at `cap` of wall time, up to `BURST`, and a spin
//! never runs longer than the tokens left.

use std::cell::Cell;
use std::time::{Duration, Instant};

/// The smallest window worth spinning; a grow from none starts here.
const MIN: Duration = Duration::from_micros(5);
/// How much spinning the bucket holds when it is full.
const BURST: Duration = Duration::from_millis(10);

pub(crate) struct Governor {
    max: Duration,
    window: Cell<Duration>,
    /// Share of wall time spinning may take, in 1/1024.
    cap: u32,
    tokens: Cell<Duration>,
    refilled: Cell<Instant>,
}

impl Governor {
    pub(crate) fn new(max: Duration, cap_pct: u32) -> Self {
        Self {
            max,
            window: Cell::new(max),
            cap: (cap_pct.min(100) * 1024 / 100).max(1),
            tokens: Cell::new(BURST),
            refilled: Cell::new(Instant::now()),
        }
    }

    /// How long to spin now: the window, within the tokens left.
    pub(crate) fn window(&self, now: Instant) -> Duration {
        let earned = now.saturating_duration_since(self.refilled.get()) * self.cap / 1024;
        self.refilled.set(now);
        let tokens = (self.tokens.get() + earned).min(BURST);
        self.tokens.set(tokens);
        let w = self.window.get().min(tokens);
        if w < MIN {
            Duration::ZERO
        } else {
            w
        }
    }

    /// The worker spun for `spun`.
    pub(crate) fn spun(&self, spun: Duration) {
        self.tokens.set(self.tokens.get().saturating_sub(spun));
    }

    /// The worker slept, and was idle `idle` in all since it ran out of work.
    pub(crate) fn slept(&self, idle: Duration) {
        let w = self.window.get();
        if idle <= self.max {
            self.window.set((w * 2).max(MIN).min(self.max));
        } else if idle > self.max * 2 {
            let half = w / 2;
            self.window
                .set(if half < MIN { Duration::ZERO } else { half });
        }
    }

    #[cfg(test)]
    fn current(&self) -> Duration {
        self.window.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Duration = Duration::from_micros(200);

    #[test]
    fn long_idles_shrink_the_window_to_none_and_short_ones_grow_it_back() {
        let g = Governor::new(MAX, 100);
        for _ in 0..10 {
            g.slept(Duration::from_millis(5));
        }
        assert_eq!(g.current(), Duration::ZERO);
        g.slept(Duration::from_micros(100));
        assert_eq!(g.current(), MIN);
        for _ in 0..10 {
            g.slept(Duration::from_micros(100));
        }
        assert_eq!(g.current(), MAX);
    }

    #[test]
    fn idles_between_one_and_two_maxima_change_nothing() {
        let g = Governor::new(MAX, 100);
        g.slept(Duration::from_millis(1));
        let w = g.current();
        for _ in 0..10 {
            g.slept(Duration::from_micros(300));
        }
        assert_eq!(g.current(), w);
    }

    #[test]
    fn the_cap_bounds_spinning() {
        let g = Governor::new(MAX, 10);
        let t = Instant::now();
        // Spin the bucket dry.
        let mut spent = Duration::ZERO;
        while spent < BURST {
            let w = g.window(t);
            if w.is_zero() {
                break;
            }
            g.spun(w);
            spent += w;
        }
        assert_eq!(g.window(t), Duration::ZERO);
        // 1 ms later, 10% of it: 100 us.
        let w = g.window(t + Duration::from_millis(1));
        assert!(
            w >= Duration::from_micros(95) && w <= Duration::from_micros(100),
            "{w:?}"
        );
    }
}
