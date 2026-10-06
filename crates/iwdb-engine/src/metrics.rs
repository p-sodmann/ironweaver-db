//! Metric primitives (step 16c, ADR 0050): a duration [`Histogram`] with
//! fixed buckets, made of atomics, and its [`HistogramSnapshot`].
//!
//! No exporter and no dependency: the storage and query crates record into
//! histograms they own, `iwdb-query` collects snapshots into its metric
//! families, and the server writes them in Prometheus' text format. Every
//! duration histogram has the same buckets ([`BUCKETS_MICROS`]), so
//! snapshots of several namespaces add up ([`HistogramSnapshot::merge`]).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The buckets' upper bounds, in microseconds: 0.5 ms to 30 s. A last
/// bucket, `+Inf`, takes the rest.
pub const BUCKETS_MICROS: [u64; 15] = [
    500, 1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000, 2_500_000, 5_000_000,
    10_000_000, 30_000_000,
];

/// Buckets, `+Inf` included.
pub const BUCKETS: usize = BUCKETS_MICROS.len() + 1;

/// A histogram of durations: counts per bucket, their sum and number.
/// Recording is a few relaxed atomic adds, from any thread; a snapshot
/// taken while others record may be off by the observations in flight,
/// never torn within one counter.
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    sum_micros: AtomicU64,
}

impl Histogram {
    pub fn new() -> Self {
        Histogram::default()
    }

    /// Record one duration.
    pub fn observe(&self, d: Duration) {
        let micros = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        let bucket = BUCKETS_MICROS.iter().position(|&bound| micros <= bound).unwrap_or(BUCKETS - 1);
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
        }
    }
}

/// A histogram's counts at one moment: per bucket (not cumulative), with
/// the last bucket `+Inf`, and the sum of the observations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistogramSnapshot {
    pub buckets: [u64; BUCKETS],
    pub sum_micros: u64,
}

impl HistogramSnapshot {
    /// The number of observations.
    pub fn count(&self) -> u64 {
        self.buckets.iter().fold(0u64, |a, &b| a.saturating_add(b))
    }

    /// Add `other`'s observations (another namespace's, say).
    pub fn merge(&mut self, other: &HistogramSnapshot) {
        for (a, b) in self.buckets.iter_mut().zip(other.buckets) {
            *a = a.saturating_add(b);
        }
        self.sum_micros = self.sum_micros.saturating_add(other.sum_micros);
    }

    /// The counts up to and including each bucket, as Prometheus wants
    /// them (the last is [`count`](Self::count)).
    pub fn cumulative(&self) -> [u64; BUCKETS] {
        let mut total = 0u64;
        self.buckets.map(|b| {
            total = total.saturating_add(b);
            total
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_land_in_the_first_bucket_that_holds_them() {
        let h = Histogram::new();
        h.observe(Duration::from_micros(500));
        h.observe(Duration::from_micros(501));
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_secs(60));
        let s = h.snapshot();
        assert_eq!(s.buckets[0], 1);
        assert_eq!(s.buckets[1], 1);
        assert_eq!(s.buckets[3], 1);
        assert_eq!(s.buckets[BUCKETS - 1], 1);
        assert_eq!(s.count(), 4);
        assert_eq!(s.sum_micros, 500 + 501 + 3_000 + 60_000_000);
        assert_eq!(s.cumulative()[BUCKETS - 1], 4);
        assert_eq!(s.cumulative()[1], 2);
    }

    #[test]
    fn snapshots_merge() {
        let (a, b) = (Histogram::new(), Histogram::new());
        a.observe(Duration::from_millis(1));
        b.observe(Duration::from_millis(1));
        b.observe(Duration::from_secs(1));
        let mut s = a.snapshot();
        s.merge(&b.snapshot());
        assert_eq!(s.count(), 3);
        assert_eq!(s.buckets[1], 2);
        assert_eq!(s.sum_micros, 1_002_000);
    }
}
