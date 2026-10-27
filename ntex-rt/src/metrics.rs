//! Runtime, arbiter, and blocking thread pool metrics.
//!
//! Use [`Histogram`] for duration distributions, [`LatencyMetrics`] for arbiter
//! scheduling latency, [`PoolMetrics`] for blocking pool activity, and
//! [`PingRecord`] for arbiter ping results.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_timer::Delay;

pub use crate::pool::PoolMetrics;
pub use crate::system::PingRecord;

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
/// Buckets are sampled independently, so a concurrent snapshot need not
/// represent one instant in time. Counts and arithmetic wrap on overflow.
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
    ///
    /// `prev` should be an earlier snapshot of the same histogram. Subtraction
    /// wraps to account for counter overflow rather than saturating at zero.
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
        if d < Duration::from_micros(FIRST_BUCKET_US) {
            0
        } else {
            let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
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
/// Fields are sampled independently and may be slightly inconsistent during
/// concurrent updates.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LatencyMetrics {
    /// Sampling interval.
    pub interval: Duration,
    /// Lateness of the most recent sample, or zero before the first sample.
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
    /// Total lateness accumulated by recovered stalls.
    ///
    /// Each recovery contributes the gap between samples minus the sampling
    /// interval. A stall still in progress is not included.
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
#[path = "../tests/unit/metrics.rs"]
mod tests;
