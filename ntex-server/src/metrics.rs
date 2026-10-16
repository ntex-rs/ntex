use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::{cell::RefCell, time::Duration, time::Instant};

use ntex_io::{IoMetrics, IoMetricsSnapshot};

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
}

/// Metrics of a single worker.
///
/// A restarted worker keeps the metrics of the worker it replaces.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
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
    /// I/O statistics of connections created on the worker, backpressure and
    /// buffered bytes.
    pub io: IoMetricsSnapshot,
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
#[derive(Debug, Default)]
struct Period {
    count: AtomicU64,
    // nanoseconds
    total: AtomicU64,
    // nanoseconds since epoch plus one, zero if not in a period
    since: AtomicU64,
}

impl Period {
    // single writer
    fn set(&self, epoch: Instant, on: bool) {
        let since = self.since.load(Ordering::Relaxed);
        let now = as_nanos(epoch.elapsed()) + 1;
        if on && since == 0 {
            self.count.fetch_add(1, Ordering::Relaxed);
            self.since.store(now, Ordering::Relaxed);
        } else if !on && since != 0 {
            self.total
                .fetch_add(now.saturating_sub(since), Ordering::Relaxed);
            self.since.store(0, Ordering::Relaxed);
        }
    }

    fn get(&self, epoch: Instant) -> (u64, Duration) {
        let since = self.since.load(Ordering::Relaxed);
        let mut total = self.total.load(Ordering::Relaxed);
        if since != 0 {
            total += (as_nanos(epoch.elapsed()) + 1).saturating_sub(since);
        }
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
        }
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
                }
            })
            .collect();

        let mut metrics = ServerMetrics {
            workers,
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
    io: Arc<IoMetrics>,
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
            io: IoMetrics::new(),
        }
    }

    /// Metrics slot of the worker running on the current thread.
    pub(crate) fn current() -> Option<Arc<WorkerSlot>> {
        CURRENT_WORKER.with(|w| w.borrow().clone())
    }

    /// Installs the slot for the current thread, connections created on the
    /// thread afterwards report I/O statistics to the slot.
    pub(crate) fn set_current(slot: Option<Arc<WorkerSlot>>) {
        if let Some(ref slot) = slot {
            IoMetrics::set_current(slot.io.clone());
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
}
