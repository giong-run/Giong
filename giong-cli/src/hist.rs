//! A latency histogram: 128 sub-buckets per power of two (under 0.8%
//! error), values in nanoseconds, exact below 128 ns.
//!
//! The same design as the benchmark lab's load generator
//! (`bench/lab/loadgen`), so `giong bench` and the lab report comparable
//! percentiles. This file uses std only and nothing else of giong-cli, so the
//! lab can include it with `#[path]` instead of keeping its own copy.

const SUB: u64 = 128;
const BUCKETS: usize = (SUB + 57 * SUB + SUB) as usize;

#[derive(Clone, Debug)]
pub struct Hist {
    counts: Vec<u64>,
    n: u64,
    max: u64,
}

impl Default for Hist {
    fn default() -> Self {
        Hist::new()
    }
}

impl Hist {
    pub fn new() -> Self {
        Hist {
            counts: vec![0; BUCKETS],
            n: 0,
            max: 0,
        }
    }

    fn index(v: u64) -> usize {
        if v < SUB {
            return v as usize;
        }
        let e = 63 - u64::from(v.leading_zeros()); // >= 7
        let m = (v >> (e - 7)) - SUB;
        (SUB + (e - 7) * SUB + m) as usize
    }

    fn lower(i: usize) -> u64 {
        let i = i as u64;
        if i < SUB {
            return i;
        }
        let e = (i - SUB) / SUB + 7;
        let m = (i - SUB) % SUB;
        (SUB + m) << (e - 7)
    }

    pub fn add(&mut self, v: u64) {
        self.counts[Self::index(v)] += 1;
        self.n += 1;
        self.max = self.max.max(v);
    }

    pub fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        self.n += o.n;
        self.max = self.max.max(o.max);
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    /// The value at quantile `q` (0.99 for p99): the midpoint of its bucket,
    /// never above the largest value seen; 0 when empty.
    pub fn quantile(&self, q: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let rank = ((q * self.n as f64).ceil() as u64).clamp(1, self.n);
        let mut seen = 0;
        for (i, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                let lo = Self::lower(i);
                let hi = Self::lower(i + 1).max(lo + 1);
                return ((lo + hi) / 2).min(self.max);
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_round_trip_and_stay_within_one_percent() {
        for v in [
            0u64,
            1,
            127,
            128,
            129,
            255,
            256,
            1000,
            123_456,
            10_000_000_000,
            u64::MAX / 2,
        ] {
            let i = Hist::index(v);
            assert!(i < BUCKETS, "{v}");
            let lo = Hist::lower(i);
            let hi = Hist::lower(i + 1);
            assert!(lo <= v && v < hi.max(lo + 1), "{v}: [{lo}, {hi})");
            assert!((hi - lo) as f64 <= (lo as f64 / 127.0).max(1.0), "{v}");
        }
        assert!(Hist::index(u64::MAX) < BUCKETS);
    }

    #[test]
    fn quantiles() {
        let mut h = Hist::new();
        assert_eq!(h.quantile(0.99), 0);
        for v in 1..=1000u64 {
            h.add(v * 1000);
        }
        let near = |got: u64, want: u64| (got as f64 - want as f64).abs() / want as f64 <= 0.008;
        assert!(near(h.quantile(0.5), 500_000), "{}", h.quantile(0.5));
        assert!(near(h.quantile(0.99), 990_000), "{}", h.quantile(0.99));
        assert!(near(h.quantile(1.0), 1_000_000));
        assert_eq!(h.max(), 1_000_000);
        let mut other = Hist::new();
        other.add(5_000_000);
        h.merge(&other);
        assert_eq!((h.count(), h.max()), (1001, 5_000_000));
        assert!(near(h.quantile(1.0), 5_000_000));
    }
}
