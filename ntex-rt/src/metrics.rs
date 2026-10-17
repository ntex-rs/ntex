use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_timer::Delay;

/// Number of [`Histogram`] buckets.
pub const HISTOGRAM_BUCKETS: usize = 16;

/// upper bound of the first bucket, in microseconds
const FIRST_BUCKET_US: u64 = 16;

/// Snapshot of a duration histogram.
///
/// Bucket `0` counts durations below 16µs, bucket `i` counts durations in
/// `[16µs * 2^(i-1), 16µs * 2^i)`, and the last bucket counts durations of
/// 262.144ms and longer.
///
/// Counts are totals since the histogram was created, the distribution over an
/// interval is the [`delta`](Self::delta) of two snapshots.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Histogram {
    buckets: [u64; HISTOGRAM_BUCKETS],
}

impl Histogram {
    /// Returns the bucket counts.
    pub fn buckets(&self) -> &[u64; HISTOGRAM_BUCKETS] {
        &self.buckets
    }

    /// Returns the total number of recorded durations.
    pub fn count(&self) -> u64 {
        self.buckets.iter().fold(0, |acc, c| acc.wrapping_add(*c))
    }

    /// Returns the duration range of a bucket.
    ///
    /// The lower bound is inclusive, the upper bound is exclusive and `None`
    /// for the last bucket.
    ///
    /// # Panics
    ///
    /// Panics if `idx` is not less than [`HISTOGRAM_BUCKETS`].
    pub fn bucket_range(idx: usize) -> (Duration, Option<Duration>) {
        assert!(idx < HISTOGRAM_BUCKETS, "bucket index out of range");
        let upper = |i: usize| Duration::from_micros(FIRST_BUCKET_US << i);
        let lower = if idx == 0 { Duration::ZERO } else { upper(idx - 1) };
        if idx == HISTOGRAM_BUCKETS - 1 {
            (lower, None)
        } else {
            (lower, Some(upper(idx)))
        }
    }

    /// Returns the counts recorded since the `prev` snapshot.
    #[must_use]
    pub fn delta(&self, prev: &Histogram) -> Histogram {
        let mut buckets = self.buckets;
        for (b, p) in buckets.iter_mut().zip(prev.buckets.iter()) {
            *b = b.wrapping_sub(*p);
        }
        Histogram { buckets }
    }

    /// Returns the sum of two histograms, for example to combine the
    /// histograms of several runtimes.
    #[must_use]
    pub fn merge(&self, other: &Histogram) -> Histogram {
        let mut buckets = self.buckets;
        for (b, o) in buckets.iter_mut().zip(other.buckets.iter()) {
            *b = b.wrapping_add(*o);
        }
        Histogram { buckets }
    }
}

/// Duration histogram updated by a single thread.
#[derive(Debug, Default)]
pub(crate) struct AtomicHistogram {
    buckets: [AtomicU64; HISTOGRAM_BUCKETS],
}

impl AtomicHistogram {
    fn bucket(d: Duration) -> usize {
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        if us < FIRST_BUCKET_US {
            0
        } else {
            // 16µs..32µs is bucket 1
            let idx = (63 - us.leading_zeros() as usize) - 3;
            idx.min(HISTOGRAM_BUCKETS - 1)
        }
    }

    /// Records a duration, must be called by a single thread.
    pub(crate) fn record(&self, d: Duration) {
        let b = &self.buckets[Self::bucket(d)];
        b.store(b.load(Ordering::Relaxed).wrapping_add(1), Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> Histogram {
        let mut buckets = [0; HISTOGRAM_BUCKETS];
        for (b, a) in buckets.iter_mut().zip(self.buckets.iter()) {
            *b = a.load(Ordering::Relaxed);
        }
        Histogram { buckets }
    }
}

/// Snapshot of an arbiter's scheduling latency.
///
/// The arbiter runs a task that sleeps for `interval` and records how late it
/// wakes up. Lateness includes the time the runtime spent running other tasks
/// and handling driver events, and the timer's own wake-up latency.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LatencyMetrics {
    /// Sampling interval.
    pub interval: Duration,
    /// Lateness of the most recent sample.
    pub last: Duration,
    /// Lateness of all samples.
    pub histogram: Histogram,
    /// Time of the most recent sample, `None` if no sample has been recorded.
    ///
    /// A time much older than `interval` means the arbiter is blocked.
    pub last_sample: Option<Instant>,
    /// Number of stalls detected by the watchdog.
    ///
    /// An arbiter is stalled when it has not recorded a sample for longer
    /// than `interval` plus the
    /// [`ping_threshold`](crate::Builder::ping_threshold).
    pub stalls: u64,
    /// Total duration of finished stalls.
    pub stall_duration: Duration,
    /// Whether the arbiter is currently stalled.
    pub stalled: bool,
}

/// Change of an arbiter stall state.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Stall {
    None,
    /// Arbiter stalled, time since the last sample
    Started(Duration),
    /// Arbiter recovered, duration of the stall
    Recovered(Duration),
}

#[derive(Debug, Default)]
pub(crate) struct LatencyStats {
    // in nanoseconds, zero if sampling is disabled
    interval: AtomicU64,
    last: AtomicU64,
    // nanoseconds since `base`, plus one, zero before the first sample
    last_sample: AtomicU64,
    base: OnceLock<Instant>,
    histogram: AtomicHistogram,
    // written by the watchdog thread only
    stalls: AtomicU64,
    stall_duration: AtomicU64,
    stalled: AtomicBool,
    stalled_at: AtomicU64,
}

fn as_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl LatencyStats {
    fn record(&self, lateness: Duration, now: Instant) {
        let base = *self.base.get_or_init(|| now);
        self.histogram.record(lateness);
        self.last.store(as_nanos(lateness), Ordering::Relaxed);
        self.last_sample
            .store(as_nanos(now - base).saturating_add(1), Ordering::Relaxed);
    }

    pub(crate) fn metrics(&self) -> Option<LatencyMetrics> {
        let interval = self.interval.load(Ordering::Relaxed);
        if interval == 0 {
            return None;
        }
        let last_sample = match self.last_sample.load(Ordering::Relaxed) {
            0 => None,
            n => self
                .base
                .get()
                .map(|base| *base + Duration::from_nanos(n - 1)),
        };
        Some(LatencyMetrics {
            interval: Duration::from_nanos(interval),
            last: Duration::from_nanos(self.last.load(Ordering::Relaxed)),
            histogram: self.histogram.snapshot(),
            last_sample,
            stalls: self.stalls.load(Ordering::Relaxed),
            stall_duration: Duration::from_nanos(self.stall_duration.load(Ordering::Relaxed)),
            stalled: self.stalled.load(Ordering::Relaxed),
        })
    }

    /// Checks whether the arbiter stopped recording samples.
    ///
    /// Must be called from a single thread.
    pub(crate) fn check(&self, now: Instant, threshold: Duration) -> Stall {
        let interval = Duration::from_nanos(self.interval.load(Ordering::Relaxed));
        let Some(base) = self.base.get() else {
            return Stall::None;
        };
        if interval.is_zero() {
            return Stall::None;
        }

        // sampler start time until the first sample
        let last = self.last_sample.load(Ordering::Relaxed).saturating_sub(1);
        if self.stalled.load(Ordering::Relaxed) {
            let prev = self.stalled_at.load(Ordering::Relaxed);
            if last == prev {
                return Stall::None;
            }
            let gap = Duration::from_nanos(last.saturating_sub(prev));
            let stall = gap.saturating_sub(interval);
            self.stalled.store(false, Ordering::Relaxed);
            self.stall_duration.store(
                self.stall_duration
                    .load(Ordering::Relaxed)
                    .saturating_add(as_nanos(stall)),
                Ordering::Relaxed,
            );
            Stall::Recovered(stall)
        } else {
            let age = now.saturating_duration_since(*base + Duration::from_nanos(last));
            if age <= interval + threshold {
                return Stall::None;
            }
            self.stalled_at.store(last, Ordering::Relaxed);
            self.stalled.store(true, Ordering::Relaxed);
            self.stalls.store(
                self.stalls.load(Ordering::Relaxed).wrapping_add(1),
                Ordering::Relaxed,
            );
            Stall::Started(age)
        }
    }
}

/// Samples the scheduling latency of the current arbiter.
pub(crate) fn start_latency_sampler(stats: &Arc<LatencyStats>, interval_ms: usize) {
    if interval_ms == 0 {
        return;
    }
    let interval = Duration::from_millis(interval_ms as u64);
    stats.base.get_or_init(Instant::now);
    stats.interval.store(as_nanos(interval), Ordering::Relaxed);

    let stats = stats.clone();
    crate::spawn_detached(async move {
        loop {
            let deadline = Instant::now() + interval;
            Delay::new(interval).await;
            let now = Instant::now();
            stats.record(now.saturating_duration_since(deadline), now);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets() {
        let bucket = |us| AtomicHistogram::bucket(Duration::from_micros(us));
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(15), 0);
        assert_eq!(bucket(16), 1);
        assert_eq!(bucket(31), 1);
        assert_eq!(bucket(32), 2);
        assert_eq!(bucket(262_143), 14);
        assert_eq!(bucket(262_144), 15);
        assert_eq!(AtomicHistogram::bucket(Duration::MAX), 15);

        for i in 0..HISTOGRAM_BUCKETS {
            let (lower, upper) = Histogram::bucket_range(i);
            assert_eq!(AtomicHistogram::bucket(lower), i);
            if let Some(upper) = upper {
                assert_eq!(AtomicHistogram::bucket(upper), i + 1);
                assert_eq!(
                    AtomicHistogram::bucket(upper.checked_sub(Duration::from_nanos(1)).unwrap()),
                    i
                );
            }
        }
        assert_eq!(Histogram::bucket_range(15).1, None);
    }

    #[test]
    fn histogram_delta() {
        let h = AtomicHistogram::default();
        h.record(Duration::from_micros(1));
        let prev = h.snapshot();
        h.record(Duration::from_micros(1));
        h.record(Duration::from_millis(1));
        let cur = h.snapshot();
        assert_eq!(cur.count(), 3);

        let delta = cur.delta(&prev);
        assert_eq!(delta.count(), 2);
        assert_eq!(delta.buckets()[0], 1);
        assert_eq!(
            delta.buckets()[AtomicHistogram::bucket(Duration::from_millis(1))],
            1
        );
        assert_eq!(prev.merge(&delta), cur);
    }

    #[test]
    fn latency_stats() {
        let stats = LatencyStats::default();
        assert_eq!(stats.metrics(), None);

        stats.interval.store(100, Ordering::Relaxed);
        let m = stats.metrics().unwrap();
        assert_eq!((m.last_sample, m.histogram.count()), (None, 0));

        let now = Instant::now();
        stats.record(Duration::from_millis(2), now);
        let m = stats.metrics().unwrap();
        assert_eq!(m.interval, Duration::from_nanos(100));
        assert_eq!(m.last, Duration::from_millis(2));
        assert_eq!(m.last_sample, Some(now));
        assert_eq!(m.histogram.count(), 1);
    }

    #[test]
    fn stall_check() {
        let ms = Duration::from_millis;
        let threshold = ms(20);
        let stats = LatencyStats::default();
        let t0 = Instant::now();
        assert_eq!(stats.check(t0 + ms(100), threshold), Stall::None);

        stats.base.set(t0).unwrap();
        stats.interval.store(as_nanos(ms(10)), Ordering::Relaxed);
        assert_eq!(stats.check(t0 + ms(30), threshold), Stall::None);
        assert_eq!(stats.check(t0 + ms(31), threshold), Stall::Started(ms(31)));
        assert_eq!(stats.check(t0 + ms(50), threshold), Stall::None);
        let m = stats.metrics().unwrap();
        assert_eq!((m.stalls, m.stall_duration, m.stalled), (1, ms(0), true));

        stats.record(ms(90), t0 + ms(100));
        assert_eq!(
            stats.check(t0 + ms(101), threshold),
            Stall::Recovered(ms(90))
        );
        assert_eq!(stats.check(t0 + ms(120), threshold), Stall::None);
        let m = stats.metrics().unwrap();
        assert_eq!((m.stalls, m.stall_duration, m.stalled), (1, ms(90), false));

        assert_eq!(stats.check(t0 + ms(131), threshold), Stall::Started(ms(31)));
        assert_eq!(stats.metrics().unwrap().stalls, 2);
    }
}
