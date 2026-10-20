use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Mutex, OnceLock};
use std::{cell::RefCell, panic::Location, time::Duration, time::Instant};

use ntex_bytes::{PageCacheMetrics, PageCacheSnapshot};
use ntex_io::{IoConfigMetrics, IoMetricsGroup, IoMetricsSnapshot};
use ntex_rt::{Arbiter, Histogram, LatencyMetrics};

use crate::WorkerId;

/// Snapshot of server metrics.
///
/// Counters and durations are totals since the server started. Rates over an
/// interval are computed from the deltas of two snapshots.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ServerMetrics {
    /// Per-worker metrics, indexed by worker id.
    pub workers: Vec<WorkerMetrics>,
    /// Per-listener metrics, empty for servers without listeners.
    pub listeners: Vec<ListenerMetrics>,
    /// Number of times the accept loop stopped accepting connections, because
    /// no worker was available or the server was paused.
    pub accept_pauses: u64,
    /// Total time the accept loop did not accept connections, including the
    /// current pause.
    pub accept_paused_duration: Duration,
    /// Number of accepted connections waiting in the accept loop for an
    /// available worker.
    pub accept_backlog: usize,
    /// Number of items sent to the worker on their incoming CPU, see
    /// [`LoadBalancing::IncomingCpu`](crate::LoadBalancing::IncomingCpu).
    pub incoming_cpu_hits: u64,
    /// Number of items with a known incoming CPU that were sent to another
    /// worker, because no worker is placed on that CPU or its worker was
    /// busier than other workers.
    pub incoming_cpu_misses: u64,
    /// Number of items taken from the queue of a failed, unavailable or
    /// blocked worker and sent to another worker.
    pub rescheduled: u64,
}

/// Metrics of a single worker.
///
/// A restarted worker keeps the metrics of the worker it replaces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WorkerMetrics {
    /// Worker id.
    pub id: WorkerId,
    /// Number of items, connections for network servers, the worker received.
    pub items: u64,
    /// Number of open connections, network servers only.
    pub active_connections: usize,
    /// Number of times the worker service stopped being ready, e.g. it reached
    /// the maximum number of connections.
    pub not_ready: u64,
    /// Total time the worker service was not ready, including the current
    /// period.
    pub not_ready_duration: Duration,
    /// Number of times the worker was restarted after a failure.
    pub restarts: u64,
    /// I/O statistics of connections created on the worker, one entry per
    /// listener configuration and per configuration with
    /// `IoConfig::set_metrics()` enabled.
    pub io: Vec<IoConfigMetrics>,
    /// I/O statistics of the worker's other connections, e.g. client
    /// connections of configurations without their own metrics.
    pub io_other: IoMetricsSnapshot,
    /// Statistics of the worker thread's buffer page cache.
    ///
    /// The metrics are single-writer. While a failed worker's thread is still
    /// releasing pages and its replacement already runs, updates can be lost,
    /// so after a worker restart the statistics may be slightly off.
    pub page_cache: PageCacheSnapshot,
    /// Statistics of the worker's runtime.
    pub runtime: RuntimeMetrics,
}

/// Runtime statistics of a worker.
///
/// Counters, durations and histograms are totals over all runtimes the
/// worker slot ran on, a restarted worker continues the totals of the worker
/// it replaces. Values a runtime backend does not provide are `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct RuntimeMetrics {
    /// Total time the runtime was busy running tasks.
    ///
    /// The `busy_duration` delta divided by the elapsed time is the worker's
    /// utilization. On tokio it is available only for current-thread
    /// runtimes.
    pub busy_duration: Option<Duration>,
    /// Number of runtime turns that ran scheduled tasks. Not available on
    /// tokio.
    pub polls: Option<u64>,
    /// Number of runtime turns that left scheduled tasks for the next turn.
    /// Not available on tokio.
    pub polls_again: Option<u64>,
    /// Number of times a task woke itself while it was polled. Neon only.
    pub self_wakes: Option<u64>,
    /// Durations of runtime turns. Not available on tokio.
    pub poll_histogram: Option<Histogram>,
    /// How long the current runtime turn has been running, `None` while the
    /// runtime is idle or the backend does not track turns.
    pub current_turn: Option<Duration>,
    /// Spawn location of the task the runtime is polling now. Neon only.
    pub current_task: Option<&'static Location<'static>>,
    /// Scheduling latency and stalls, `None` if latency sampling is disabled,
    /// see `ntex_rt::Builder::latency_interval()`.
    ///
    /// `histogram`, `stalls` and `stall_duration` are totals over all
    /// runtimes of the worker slot, the other fields describe the current
    /// runtime.
    pub latency: Option<LatencyMetrics>,
}

/// Metrics of a single listener.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ListenerMetrics {
    /// Service name the listener was registered with.
    pub name: String,
    /// Local address of the listener.
    pub addr: String,
    /// Number of accepted connections.
    pub accepted: u64,
    /// Number of per-connection accept errors, e.g. the connection was reset
    /// before it was accepted.
    pub connection_errors: u64,
    /// Number of other accept errors, e.g. the process ran out of file
    /// descriptors. The listener stops accepting for a short time after each.
    pub resource_errors: u64,
}

fn as_nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Accumulated period, e.g. time the worker was not ready.
///
/// The total and the open period are packed into one word, so a reader sees a
/// coherent value with a single load: `state >> 2` is the total in
/// nanoseconds, minus the start of the open period if `OPEN` is set. Values
/// are nanoseconds since the epoch, modulo 2^62 (about 146 years).
#[derive(Debug, Default)]
struct Period {
    count: AtomicU64,
    state: AtomicU64,
}

impl Period {
    const OPEN: u64 = 1;
    // the open period is ending, its end time is not stored yet
    const ENDING: u64 = 2;
    const MASK: u64 = u64::MAX >> 2;

    // single writer
    fn set(&self, epoch: Instant, on: bool) {
        let state = self.state.load(Ordering::Relaxed);
        // called on every readiness check, read the clock only on changes
        if on == (state & Self::OPEN != 0) {
            return;
        }
        if on {
            self.count.fetch_add(1, Ordering::Relaxed);
            let acc = (state >> 2).wrapping_sub(as_nanos(epoch.elapsed()));
            self.state.store((acc << 2) | Self::OPEN, Ordering::Release);
        } else {
            // readers wait for the end time instead of reading a later clock
            self.state.store(state | Self::ENDING, Ordering::Relaxed);
            fence(Ordering::SeqCst);
            let total = (state >> 2).wrapping_add(as_nanos(epoch.elapsed()));
            self.state.store(total << 2, Ordering::Release);
        }
    }

    fn get(&self, epoch: Instant) -> (u64, Duration) {
        let total = loop {
            // acquire, the clock is read after the writer's
            let state = self.state.load(Ordering::Acquire);
            if state & Self::OPEN == 0 {
                break state >> 2;
            }
            if state & Self::ENDING == 0 {
                let now = as_nanos(epoch.elapsed());
                fence(Ordering::SeqCst);
                // the period did not start ending before the clock was read
                if self.state.load(Ordering::Relaxed) == state {
                    break (state >> 2).wrapping_add(now) & Self::MASK;
                }
            }
            std::thread::yield_now();
        };
        (
            self.count.load(Ordering::Relaxed),
            Duration::from_nanos(total),
        )
    }
}

#[derive(Debug)]
pub(crate) struct Metrics {
    epoch: Instant,
    workers: Box<[Arc<WorkerSlot>]>,
    accept: OnceLock<AcceptSlot>,
    incoming_cpu_hits: AtomicU64,
    incoming_cpu_misses: AtomicU64,
    rescheduled: AtomicU64,
}

impl Metrics {
    pub(crate) fn new(workers: usize) -> Self {
        let epoch = Instant::now();
        Self {
            epoch,
            workers: (0..workers)
                .map(|_| Arc::new(WorkerSlot::new(epoch)))
                .collect(),
            accept: OnceLock::new(),
            incoming_cpu_hits: AtomicU64::new(0),
            incoming_cpu_misses: AtomicU64::new(0),
            rescheduled: AtomicU64::new(0),
        }
    }

    /// An item with a known incoming CPU was sent to the worker on that CPU,
    /// or to another worker.
    pub(crate) fn incoming_cpu(&self, hit: bool) {
        let counter = if hit {
            &self.incoming_cpu_hits
        } else {
            &self.incoming_cpu_misses
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Items were taken from a worker's queue to be sent to other workers.
    pub(crate) fn rescheduled(&self, n: u64) {
        self.rescheduled.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn worker(&self, id: WorkerId) -> Option<Arc<WorkerSlot>> {
        self.workers.get(id.0).cloned()
    }

    /// Registers the listeners, in accept loop order.
    ///
    /// Only the first call has an effect.
    pub(crate) fn listeners<I>(&self, listeners: I) -> &AcceptSlot
    where
        I: IntoIterator<Item = (String, String)>,
    {
        self.accept.get_or_init(|| AcceptSlot {
            epoch: self.epoch,
            listeners: listeners
                .into_iter()
                .map(|(name, addr)| ListenerSlot {
                    name,
                    addr,
                    ..Default::default()
                })
                .collect(),
            paused: Period::default(),
            backlog: AtomicUsize::new(0),
        })
    }

    pub(crate) fn snapshot(&self) -> ServerMetrics {
        let workers = self
            .workers
            .iter()
            .enumerate()
            .map(|(id, w)| {
                let (not_ready, not_ready_duration) = w.not_ready.get(self.epoch);
                WorkerMetrics {
                    id: WorkerId(id),
                    items: w.items.load(Ordering::Relaxed),
                    active_connections: w.active.load(Ordering::Relaxed),
                    not_ready,
                    not_ready_duration,
                    restarts: w.restarts.load(Ordering::Relaxed),
                    io: w.io.snapshot(),
                    io_other: w.io.other().snapshot(),
                    page_cache: w.page_cache.snapshot(),
                    runtime: w.runtime_metrics(),
                }
            })
            .collect();

        let mut metrics = ServerMetrics {
            workers,
            incoming_cpu_hits: self.incoming_cpu_hits.load(Ordering::Relaxed),
            incoming_cpu_misses: self.incoming_cpu_misses.load(Ordering::Relaxed),
            rescheduled: self.rescheduled.load(Ordering::Relaxed),
            ..Default::default()
        };
        if let Some(accept) = self.accept.get() {
            (metrics.accept_pauses, metrics.accept_paused_duration) = accept.paused.get(self.epoch);
            metrics.accept_backlog = accept.backlog.load(Ordering::Relaxed);
            metrics.listeners = accept
                .listeners
                .iter()
                .map(|l| ListenerMetrics {
                    name: l.name.clone(),
                    addr: l.addr.clone(),
                    accepted: l.accepted.load(Ordering::Relaxed),
                    connection_errors: l.connection_errors.load(Ordering::Relaxed),
                    resource_errors: l.resource_errors.load(Ordering::Relaxed),
                })
                .collect();
        }
        metrics
    }
}

// separate cache lines, every worker updates its own slot
#[derive(Debug)]
#[repr(align(64))]
pub(crate) struct WorkerSlot {
    epoch: Instant,
    items: AtomicU64,
    active: AtomicUsize,
    not_ready: Period,
    restarts: AtomicU64,
    io: Arc<IoMetricsGroup>,
    page_cache: Arc<PageCacheMetrics>,
    runtime: Mutex<RuntimeSlot>,
}

/// Runtime of the worker and the totals of the runtimes it replaced.
#[derive(Debug, Default)]
struct RuntimeSlot {
    arbiter: Option<Arbiter>,
    base: RuntimeTotals,
}

#[derive(Debug, Default)]
struct RuntimeTotals {
    busy: Duration,
    polls: u64,
    polls_again: u64,
    self_wakes: u64,
    poll_histogram: Histogram,
    latency_histogram: Histogram,
    stalls: u64,
    stall_duration: Duration,
}

impl RuntimeTotals {
    fn add(&mut self, m: &RuntimeMetrics) {
        self.busy += m.busy_duration.unwrap_or_default();
        self.polls += m.polls.unwrap_or_default();
        self.polls_again += m.polls_again.unwrap_or_default();
        self.self_wakes += m.self_wakes.unwrap_or_default();
        if let Some(ref h) = m.poll_histogram {
            self.poll_histogram = self.poll_histogram.merge(h);
        }
        if let Some(ref l) = m.latency {
            self.latency_histogram = self.latency_histogram.merge(&l.histogram);
            self.stalls += l.stalls;
            self.stall_duration += l.stall_duration;
        }
    }
}

fn runtime_metrics(arb: &Arbiter) -> RuntimeMetrics {
    let hnd = arb.handle();
    RuntimeMetrics {
        busy_duration: hnd.busy_duration(),
        polls: hnd.poll_count(),
        polls_again: hnd.poll_again_count(),
        self_wakes: hnd.self_wake_count(),
        poll_histogram: hnd.poll_histogram(),
        current_turn: hnd.current_turn(),
        current_task: hnd.current_task(),
        latency: arb.latency(),
    }
}

thread_local! {
    static CURRENT_WORKER: RefCell<Option<Arc<WorkerSlot>>> = const { RefCell::new(None) };
}

impl WorkerSlot {
    fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            items: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            not_ready: Period::default(),
            restarts: AtomicU64::new(0),
            io: IoMetricsGroup::new(),
            page_cache: PageCacheMetrics::new(),
            runtime: Mutex::default(),
        }
    }

    /// Sets the arbiter the worker runs on, the totals of the previous
    /// arbiter are kept.
    pub(crate) fn set_arbiter(&self, arbiter: Arbiter) {
        let mut rt = self.runtime.lock().unwrap();
        if let Some(prev) = rt.arbiter.replace(arbiter) {
            let m = runtime_metrics(&prev);
            rt.base.add(&m);
        }
    }

    fn runtime_metrics(&self) -> RuntimeMetrics {
        let rt = self.runtime.lock().unwrap();
        let Some(ref arb) = rt.arbiter else {
            return RuntimeMetrics::default();
        };
        let base = &rt.base;
        let mut m = runtime_metrics(arb);
        m.busy_duration = m.busy_duration.map(|d| d + base.busy);
        m.polls = m.polls.map(|n| n + base.polls);
        m.polls_again = m.polls_again.map(|n| n + base.polls_again);
        m.self_wakes = m.self_wakes.map(|n| n + base.self_wakes);
        m.poll_histogram = m.poll_histogram.map(|h| h.merge(&base.poll_histogram));
        if let Some(ref mut l) = m.latency {
            l.histogram = l.histogram.merge(&base.latency_histogram);
            l.stalls += base.stalls;
            l.stall_duration += base.stall_duration;
        }
        m
    }

    /// Metrics slot of the worker running on the current thread.
    pub(crate) fn current() -> Option<Arc<WorkerSlot>> {
        CURRENT_WORKER.with(|w| w.borrow().clone())
    }

    /// Installs the slot for the current thread, connections created on the
    /// thread afterwards report I/O statistics to the slot, the thread's page
    /// cache reports to the slot.
    pub(crate) fn set_current(slot: Option<Arc<WorkerSlot>>) {
        if let Some(ref slot) = slot {
            IoMetricsGroup::set_current(slot.io.clone());
            PageCacheMetrics::set_current(slot.page_cache.clone());
        }
        CURRENT_WORKER.with(|w| *w.borrow_mut() = slot);
    }

    pub(crate) fn item(&self) {
        self.items.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn ready(&self, ready: bool) {
        self.not_ready.set(self.epoch, !ready);
    }

    pub(crate) fn restart(&self) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub(crate) fn conn_opened(&self) {
        self.active.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn conn_closed(&self) {
        self.active.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub(crate) struct AcceptSlot {
    epoch: Instant,
    listeners: Box<[ListenerSlot]>,
    paused: Period,
    backlog: AtomicUsize,
}

#[derive(Debug, Default)]
struct ListenerSlot {
    name: String,
    addr: String,
    accepted: AtomicU64,
    connection_errors: AtomicU64,
    resource_errors: AtomicU64,
}

impl AcceptSlot {
    pub(crate) fn paused(&self, paused: bool) {
        self.paused.set(self.epoch, paused);
    }

    pub(crate) fn backlog(&self, len: usize) {
        self.backlog.store(len, Ordering::Relaxed);
    }

    pub(crate) fn accepted(&self, idx: usize) {
        if let Some(l) = self.listeners.get(idx) {
            l.accepted.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn connection_error(&self, idx: usize) {
        if let Some(l) = self.listeners.get(idx) {
            l.connection_errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn resource_error(&self, idx: usize) {
        if let Some(l) = self.listeners.get(idx) {
            l.resource_errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn period() {
        let epoch = Instant::now();
        let p = Period::default();
        assert_eq!(p.get(epoch), (0, Duration::ZERO));

        p.set(epoch, true);
        p.set(epoch, true);
        std::thread::sleep(Duration::from_millis(10));
        let (cnt, cur) = p.get(epoch);
        assert_eq!(cnt, 1);
        assert!(cur >= Duration::from_millis(10));

        p.set(epoch, false);
        p.set(epoch, false);
        let (cnt, total) = p.get(epoch);
        assert_eq!(cnt, 1);
        assert!(total >= cur);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(p.get(epoch), (1, total));

        p.set(epoch, true);
        assert_eq!(p.get(epoch).0, 2);
    }

    #[test]
    fn period_is_coherent() {
        let epoch = Instant::now();
        let p = Arc::new(Period::default());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (p, stop) = (p.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    p.set(epoch, true);
                    let start = Instant::now();
                    while start.elapsed() < Duration::from_micros(20) {}
                    p.set(epoch, false);
                }
            })
        };
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut prev = Duration::ZERO;
        while Instant::now() < deadline {
            let (_, total) = p.get(epoch);
            // a completed period is not counted twice
            assert!(total >= prev, "{total:?} < {prev:?}");
            prev = total;
            // periods do not overlap, the writer is almost always in one
            let uptime = epoch.elapsed();
            assert!(total <= uptime, "{total:?} > {uptime:?}");
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn snapshot() {
        let m = Metrics::new(2);
        let w = m.worker(WorkerId(1)).unwrap();
        assert!(m.worker(WorkerId(2)).is_none());
        w.item();
        w.item();
        w.conn_opened();
        w.conn_opened();
        w.conn_closed();
        w.restart();

        let acc = m.listeners([("srv".to_string(), "127.0.0.1:1".to_string())]);
        acc.accepted(0);
        acc.connection_error(0);
        acc.resource_error(0);
        acc.resource_error(1);
        acc.backlog(3);
        // the first registration wins
        m.listeners([]);

        let s = m.snapshot();
        assert_eq!(s.workers.len(), 2);
        assert_eq!(s.workers[0], WorkerMetrics::default());
        assert_eq!(s.workers[1].id, WorkerId(1));
        assert_eq!(s.workers[1].items, 2);
        assert_eq!(s.workers[1].active_connections, 1);
        assert_eq!(s.workers[1].restarts, 1);
        assert_eq!(s.listeners.len(), 1);
        assert_eq!(s.listeners[0].name, "srv");
        assert_eq!(s.listeners[0].addr, "127.0.0.1:1");
        assert_eq!(s.listeners[0].accepted, 1);
        assert_eq!(s.listeners[0].connection_errors, 1);
        assert_eq!(s.listeners[0].resource_errors, 1);
        assert_eq!(s.accept_backlog, 3);
        assert_eq!(s.accept_pauses, 0);
    }

    /// Runtime totals are kept when a worker's arbiter is replaced.
    #[ntex::test]
    async fn runtime_restart() {
        let busy = |a: &Arbiter| {
            a.handle()
                .spawn(async {
                    std::thread::sleep(Duration::from_millis(20));
                })
                .detach();
        };

        let m = Metrics::new(1);
        let w = m.worker(WorkerId(0)).unwrap();
        assert_eq!(w.runtime_metrics(), RuntimeMetrics::default());

        let a1 = Arbiter::with_name("rt-metrics-1".to_string());
        w.set_arbiter(a1.clone());
        busy(&a1);
        ntex::time::sleep(ntex::time::Millis(250)).await;
        let m1 = w.runtime_metrics();
        let b1 = m1.busy_duration.unwrap();
        let l1 = m1.latency.clone().unwrap().histogram.count();
        assert!(b1 >= Duration::from_millis(20));
        assert!(l1 > 0);
        a1.stop();

        let a2 = Arbiter::with_name("rt-metrics-2".to_string());
        w.set_arbiter(a2.clone());
        busy(&a2);
        ntex::time::sleep(ntex::time::Millis(250)).await;
        let m2 = w.runtime_metrics();
        assert!(m2.busy_duration.unwrap() >= b1 + Duration::from_millis(20));
        assert!(m2.latency.unwrap().histogram.count() > l1);
        if let (Some(p1), Some(p2)) = (m1.polls, m2.polls) {
            assert!(p2 > p1);
        }
        a2.stop();
    }
}
