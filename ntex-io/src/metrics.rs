//! I/O load statistics.
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::{cell::RefCell, sync::Arc, time::Duration, time::Instant};

thread_local! {
    static CURRENT: RefCell<Option<Arc<IoMetrics>>> = const { RefCell::new(None) };
}

/// I/O load statistics shared by connections.
///
/// A connection reports to the metrics that were current on its thread when it
/// was created, see [`IoMetrics::current`]. Statistics are updated with relaxed
/// atomics, a snapshot is not a consistent cut across all fields.
#[derive(Debug)]
pub struct IoMetrics {
    base: Instant,
    rd: Backpressure,
    wr: Backpressure,
    read_buffered: AtomicUsize,
    write_buffered: AtomicUsize,
}

/// Backpressure statistics of one direction.
///
/// The connection-time under backpressure is the sum of all ended periods
/// plus the open ones. Each period adds its start to `starts` and its end to
/// `ends`, so the total at time `now` is `ends - starts + active * now`,
/// without any per-connection state. Times are nanoseconds since `base`.
#[derive(Debug, Default)]
struct Backpressure {
    count: AtomicU64,
    active: AtomicUsize,
    starts: AtomicU64,
    ends: AtomicU64,
}

/// A snapshot of [`IoMetrics`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct IoMetricsSnapshot {
    /// Number of times read backpressure was enabled.
    pub read_backpressure: u64,
    /// Number of times write backpressure was enabled.
    pub write_backpressure: u64,
    /// Connections with read backpressure enabled.
    pub read_backpressure_active: usize,
    /// Connections with write backpressure enabled.
    pub write_backpressure_active: usize,
    /// Total connection-time spent under read backpressure.
    pub read_backpressure_time: Duration,
    /// Total connection-time spent under write backpressure.
    pub write_backpressure_time: Duration,
    /// Bytes held in read buffers.
    pub read_buffered: usize,
    /// Bytes held in write buffers or taken by the transport but not written.
    pub write_buffered: usize,
}

impl IoMetrics {
    /// Creates new metrics.
    pub fn new() -> Arc<Self> {
        Arc::new(IoMetrics {
            base: Instant::now(),
            rd: Backpressure::default(),
            wr: Backpressure::default(),
            read_buffered: AtomicUsize::new(0),
            write_buffered: AtomicUsize::new(0),
        })
    }

    /// Returns the metrics of the current thread.
    ///
    /// Each thread has its own metrics, created on first use, unless other
    /// metrics were installed with [`IoMetrics::set_current`].
    pub fn current() -> Arc<Self> {
        CURRENT
            .try_with(|c| c.borrow_mut().get_or_insert_with(IoMetrics::new).clone())
            .unwrap_or_else(|_| IoMetrics::new())
    }

    /// Installs `metrics` as the current thread's metrics.
    ///
    /// Connections created afterwards on this thread report to `metrics`,
    /// existing connections keep reporting to the previous ones.
    pub fn set_current(metrics: Arc<Self>) {
        let _ = CURRENT.try_with(|c| *c.borrow_mut() = Some(metrics));
    }

    /// Returns a snapshot of the statistics.
    pub fn snapshot(&self) -> IoMetricsSnapshot {
        let now = self.nanos(Instant::now());
        IoMetricsSnapshot {
            read_backpressure: self.rd.count.load(Relaxed),
            write_backpressure: self.wr.count.load(Relaxed),
            read_backpressure_active: self.rd.active.load(Relaxed),
            write_backpressure_active: self.wr.active.load(Relaxed),
            read_backpressure_time: self.rd.time(now),
            write_backpressure_time: self.wr.time(now),
            read_buffered: self.read_buffered.load(Relaxed),
            write_buffered: self.write_buffered.load(Relaxed),
        }
    }

    fn nanos(&self, t: Instant) -> u64 {
        u64::try_from(t.saturating_duration_since(self.base).as_nanos()).unwrap_or(u64::MAX)
    }

    fn now(&self) -> u64 {
        self.nanos(ntex_rt::now())
    }

    pub(crate) fn rd_backpressure_on(&self) {
        self.rd.on(self.now());
    }

    pub(crate) fn rd_backpressure_off(&self) {
        self.rd.off(self.now());
    }

    pub(crate) fn wr_backpressure_on(&self) {
        self.wr.on(self.now());
    }

    pub(crate) fn wr_backpressure_off(&self) {
        self.wr.off(self.now());
    }

    pub(crate) fn update_read_buffered(&self, prev: u32, cur: u32) {
        update(&self.read_buffered, prev, cur);
    }

    pub(crate) fn update_write_buffered(&self, prev: u32, cur: u32) {
        update(&self.write_buffered, prev, cur);
    }
}

fn update(gauge: &AtomicUsize, prev: u32, cur: u32) {
    if cur > prev {
        gauge.fetch_add((cur - prev) as usize, Relaxed);
    } else if cur < prev {
        gauge.fetch_sub((prev - cur) as usize, Relaxed);
    }
}

impl Backpressure {
    fn on(&self, now: u64) {
        self.count.fetch_add(1, Relaxed);
        self.starts.fetch_add(now, Relaxed);
        self.active.fetch_add(1, Relaxed);
    }

    fn off(&self, now: u64) {
        self.ends.fetch_add(now, Relaxed);
        self.active.fetch_sub(1, Relaxed);
    }

    fn time(&self, now: u64) -> Duration {
        let active = self.active.load(Relaxed) as u64;
        let total = self
            .ends
            .load(Relaxed)
            .wrapping_add(active.wrapping_mul(now))
            .wrapping_sub(self.starts.load(Relaxed));
        // fields are read one by one, a racing update can make it negative
        if total > i64::MAX as u64 {
            Duration::ZERO
        } else {
            Duration::from_nanos(total)
        }
    }
}
