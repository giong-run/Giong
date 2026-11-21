//! The spin governor: how long a worker busy-polls before it sleeps
//! ([`Builder::spin_before_park`](crate::Builder::spin_before_park)), tuned
//! per worker from what spinning would have caught.
//!
//! After each sleep the worker knows how long it was idle in all (spin plus
//! sleep). Idle no longer than the maximum spin means the next event came
//! soon after the worker gave up: spinning longer would have caught it, so
//! the spin doubles. Idle more than twice the maximum means spinning only
//! burned the CPU: the spin halves, down to none. Between the two nothing
//! changes, so a load whose gaps sit near the maximum does not make the spin
//! swing. A spin that catches work changes nothing either.
//!
//! Measured with `bench/spin.sh` (`bench/results/spin/`): where requests come
//! further apart than twice the maximum, a worker costs what one that never
//! spins costs (2.9% of a core at 1,000 requests/s on one connection, against
//! 22% spinning 200 µs every time), with the p99 of a worker that never spins.

use std::cell::Cell;
use std::time::Duration;

/// The smallest spin worth starting; a grow from none starts here.
const MIN: Duration = Duration::from_micros(5);

pub(crate) struct Governor {
    max: Duration,
    window: Cell<Duration>,
}

impl Governor {
    pub(crate) fn new(max: Duration) -> Self {
        Self {
            max,
            window: Cell::new(max),
        }
    }

    /// How long to spin now; zero for not at all.
    pub(crate) fn window(&self) -> Duration {
        self.window.get()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Duration = Duration::from_micros(200);

    #[test]
    fn long_idles_shrink_the_spin_to_none_and_short_ones_grow_it_back() {
        let g = Governor::new(MAX);
        assert_eq!(g.window(), MAX);
        for _ in 0..10 {
            g.slept(Duration::from_millis(5));
        }
        assert_eq!(g.window(), Duration::ZERO);
        g.slept(Duration::from_micros(100));
        assert_eq!(g.window(), MIN);
        for _ in 0..10 {
            g.slept(Duration::from_micros(100));
        }
        assert_eq!(g.window(), MAX);
    }

    #[test]
    fn idles_between_one_and_two_maxima_change_nothing() {
        let g = Governor::new(MAX);
        g.slept(Duration::from_millis(1));
        let w = g.window();
        assert_eq!(w, MAX / 2);
        for _ in 0..10 {
            g.slept(Duration::from_micros(300));
        }
        assert_eq!(g.window(), w);
    }

    #[test]
    fn a_tiny_maximum_still_spins() {
        // Below MIN the maximum wins over MIN.
        let g = Governor::new(Duration::from_micros(2));
        g.slept(Duration::from_micros(1));
        assert_eq!(g.window(), Duration::from_micros(2));
    }
}
