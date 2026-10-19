//! I/O load statistics.
use std::sync::atomic::{
    AtomicU64, AtomicUsize, Ordering::Acquire, Ordering::Relaxed, Ordering::Release, fence,
};
use std::sync::{Arc, Mutex, PoisonError};
use std::{cell::RefCell, collections::HashMap, time::Duration, time::Instant};

use crate::IoConfig;

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
    static DETACHED: Arc<IoMetrics> = IoMetrics::new();
}

/// I/O metrics of one thread, one [`IoMetrics`] per opted-in [`IoConfig`].
///
/// A connection reports to the metrics of its configuration in the group that
/// was current on its thread when it was created, see
/// [`IoMetricsGroup::current`] and [`IoConfig::metrics`]. Connections created
/// with the same configuration on the thread share the metrics, for example
/// all connections accepted by one server listener on one worker.
///
/// A configuration has its own metrics if it was registered in the group with
/// [`IoMetricsGroup::register`] or enabled with
/// [`IoConfig::set_metrics`](crate::IoConfig::set_metrics). Connections of
/// other configurations report to the group's shared
/// [`other`](IoMetricsGroup::other) metrics. Metrics are kept for the lifetime
/// of the group.
#[derive(Debug)]
pub struct IoMetricsGroup {
    configs: Mutex<HashMap<usize, Entry>>,
    other: Arc<IoMetrics>,
    /// Incremented by each registration, invalidates the thread caches.
    generation: AtomicU64,
}

#[derive(Debug)]
struct Entry {
    tag: &'static str,
    service: &'static str,
    metrics: Arc<IoMetrics>,
}

/// The current thread's group and its configurations with own metrics, the
/// group's lock is taken when a configuration with metrics enabled is first
/// used on the thread, and once after each registration in the group.
/// Configurations without own metrics are not cached, they map to `other`.
struct Current {
    group: Arc<IoMetricsGroup>,
    cache: HashMap<usize, Arc<IoMetrics>>,
    /// Generation of the group when the cache was filled.
    generation: u64,
}

impl Current {
    fn new(group: Arc<IoMetricsGroup>) -> Self {
        let mut c = Current {
            generation: 0,
            group,
            cache: HashMap::new(),
        };
        c.fill();
        c
    }

    /// Refills the cache if configurations were registered since it was
    /// filled.
    fn refresh(&mut self) {
        if self.group.generation.load(Acquire) != self.generation {
            self.fill();
        }
    }

    fn fill(&mut self) {
        self.generation = self.group.generation.load(Acquire);
        self.cache = self
            .group
            .configs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(id, e)| (*id, e.metrics.clone()))
            .collect();
    }
}

/// A snapshot of the metrics of one configuration in an [`IoMetricsGroup`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct IoConfigMetrics {
    /// Id of the shared configuration, see `SharedCfg::id()`.
    pub id: usize,
    /// Tag of the shared configuration.
    pub tag: &'static str,
    /// Service name of the shared configuration.
    pub service: &'static str,
    /// I/O statistics of the configuration's connections.
    pub io: IoMetricsSnapshot,
}

impl IoMetricsGroup {
    /// Creates an empty group.
    pub fn new() -> Arc<Self> {
        Arc::new(IoMetricsGroup {
            configs: Mutex::default(),
            other: IoMetrics::new(),
            generation: AtomicU64::new(0),
        })
    }

    /// Returns the group of the current thread.
    ///
    /// Each thread has its own group, created on first use, unless another
    /// group was installed with [`IoMetricsGroup::set_current`].
    pub fn current() -> Arc<Self> {
        CURRENT
            .try_with(|c| {
                c.borrow_mut()
                    .get_or_insert_with(|| Current::new(IoMetricsGroup::new()))
                    .group
                    .clone()
            })
            .unwrap_or_else(|_| IoMetricsGroup::new())
    }

    /// Returns the metrics of `cfg` in the current thread's group.
    pub(crate) fn current_metrics(cfg: &IoConfig) -> Arc<IoMetrics> {
        CURRENT
            .try_with(|c| {
                let mut c = c.borrow_mut();
                let c = c.get_or_insert_with(|| Current::new(IoMetricsGroup::new()));
                // a configuration may have been registered on another thread
                c.refresh();
                if let Some(m) = c.cache.get(&cfg.config.id()) {
                    m.clone()
                } else if cfg.metrics_enabled() {
                    let m = c.group.entry(cfg);
                    c.cache.insert(cfg.config.id(), m.clone());
                    m
                } else {
                    c.group.other.clone()
                }
            })
            .unwrap_or_else(|_| IoMetricsGroup::new().get(cfg))
    }

    /// Installs `group` as the current thread's group.
    ///
    /// Connections created afterwards on this thread report to `group`,
    /// existing connections keep reporting to the previous one.
    ///
    /// The byte and write counters of [`IoMetrics`] are single-writer, install
    /// a group on one thread at a time.
    pub fn set_current(group: Arc<Self>) {
        let _ = CURRENT.try_with(|c| *c.borrow_mut() = Some(Current::new(group)));
    }

    /// Gives `cfg` its own metrics in this group.
    ///
    /// Connections created afterwards with `cfg`, on any thread whose current
    /// group this is, report to these metrics instead of the shared
    /// [`other`](Self::other) metrics.
    pub fn register(&self, cfg: &IoConfig) {
        let _ = self.entry(cfg);
        // threads refill their caches on the next lookup
        self.generation.fetch_add(1, Release);
    }

    /// Returns the metrics `cfg` reports to.
    ///
    /// These are the configuration's own metrics if it is registered or has
    /// metrics enabled, and the shared [`other`](Self::other) metrics
    /// otherwise.
    pub fn get(&self, cfg: &IoConfig) -> Arc<IoMetrics> {
        if cfg.metrics_enabled() {
            self.entry(cfg)
        } else {
            self.configs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&cfg.config.id())
                .map_or_else(|| self.other.clone(), |e| e.metrics.clone())
        }
    }

    /// Returns the metrics of connections whose configuration has no metrics
    /// of its own.
    pub fn other(&self) -> &Arc<IoMetrics> {
        &self.other
    }

    fn entry(&self, cfg: &IoConfig) -> Arc<IoMetrics> {
        let ctx = &cfg.config;
        self.configs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(ctx.id())
            .or_insert_with(|| Entry {
                tag: ctx.tag(),
                service: ctx.service(),
                metrics: IoMetrics::new(),
            })
            .metrics
            .clone()
    }

    /// Returns snapshots of the metrics of configurations with their own
    /// metrics, ordered by configuration id. See [`other`](Self::other) for
    /// the remaining connections.
    pub fn snapshot(&self) -> Vec<IoConfigMetrics> {
        let mut items: Vec<_> = self
            .configs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(id, e)| IoConfigMetrics {
                id: *id,
                tag: e.tag,
                service: e.service,
                io: e.metrics.snapshot(),
            })
            .collect();
        items.sort_unstable_by_key(|m| m.id);
        items
    }
}

/// I/O load statistics shared by connections.
///
/// Metrics belong to a configuration in an [`IoMetricsGroup`], see
/// [`IoConfig::metrics`]. Statistics are updated with relaxed atomics, a
/// snapshot is not a consistent cut across all fields.
///
/// The byte and write counters (`bytes_read`, `bytes_written`, `eager_writes`,
/// `eager_writes_partial`, `scheduled_writes`) are updated with a plain load
/// and store, not an atomic read-modify-write, so they must be updated by one
/// thread at a time. If the same group is installed on several threads with
/// [`IoMetricsGroup::set_current`], concurrent updates can be lost and these
/// counters undercount. The other statistics stay exact.
#[derive(Debug)]
pub struct IoMetrics {
    base: Instant,
    rd: Backpressure,
    wr: Backpressure,
    read_buffered: AtomicUsize,
    read_buffers_held: AtomicUsize,
    read_buffer_capacity: AtomicUsize,
    write_buffered: AtomicUsize,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    eager_writes: AtomicU64,
    eager_writes_partial: AtomicU64,
    scheduled_writes: AtomicU64,
    closed: [AtomicU64; CLOSE_REASONS],
}

const CLOSE_REASONS: usize = 18;

/// Reason a connection was closed.
///
/// Protocol dispatchers report it with
/// [`IoRef::set_close_reason`](crate::IoRef::set_close_reason), the first
/// reported reason is kept. It is counted in [`IoMetrics`] when the connection
/// state is released.
///
/// Applications can count their own reasons with `Custom1` to `Custom8`. Set
/// the reason before closing the connection or failing the service, a reason
/// reported by the dispatcher is not replaced.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CloseReason {
    /// The connection was idle for longer than the keep-alive timeout.
    KeepAlive,
    /// Input did not arrive within the read timeout.
    ReadTimeout,
    /// Write backpressure stayed enabled for longer than the write timeout.
    WriteTimeout,
    /// The peer closed the connection.
    PeerGone,
    /// The transport failed with an I/O error.
    Io,
    /// Incoming data could not be decoded.
    Decoder,
    /// Outgoing data could not be encoded.
    Encoder,
    /// The service failed or closed the connection.
    Service,
    /// The server is shutting down.
    Shutdown,
    /// Application-defined reason, counted in `CloseStats::custom[0]`.
    Custom1,
    /// Application-defined reason, counted in `CloseStats::custom[1]`.
    Custom2,
    /// Application-defined reason, counted in `CloseStats::custom[2]`.
    Custom3,
    /// Application-defined reason, counted in `CloseStats::custom[3]`.
    Custom4,
    /// Application-defined reason, counted in `CloseStats::custom[4]`.
    Custom5,
    /// Application-defined reason, counted in `CloseStats::custom[5]`.
    Custom6,
    /// Application-defined reason, counted in `CloseStats::custom[6]`.
    Custom7,
    /// Application-defined reason, counted in `CloseStats::custom[7]`.
    Custom8,
}

impl CloseReason {
    fn index(self) -> usize {
        match self {
            CloseReason::KeepAlive => 0,
            CloseReason::ReadTimeout => 1,
            CloseReason::WriteTimeout => 2,
            CloseReason::PeerGone => 3,
            CloseReason::Io => 4,
            CloseReason::Decoder => 5,
            CloseReason::Encoder => 6,
            CloseReason::Service => 7,
            CloseReason::Shutdown => 8,
            CloseReason::Custom1 => 9,
            CloseReason::Custom2 => 10,
            CloseReason::Custom3 => 11,
            CloseReason::Custom4 => 12,
            CloseReason::Custom5 => 13,
            CloseReason::Custom6 => 14,
            CloseReason::Custom7 => 15,
            CloseReason::Custom8 => 16,
        }
    }
}

// connections closed without a reported reason or I/O error
const CLOSE_OTHER: usize = 17;

/// Closed connections by [`CloseReason`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CloseStats {
    /// Closed connections.
    pub total: u64,
    /// Closed by the keep-alive timeout.
    pub keepalive: u64,
    /// Closed by the read timeout.
    pub read_timeout: u64,
    /// Closed by the write timeout.
    pub write_timeout: u64,
    /// Closed by the peer.
    pub peer_gone: u64,
    /// Closed by an I/O error, reported or not.
    pub io: u64,
    /// Closed because incoming data could not be decoded.
    pub decoder: u64,
    /// Closed because outgoing data could not be encoded.
    pub encoder: u64,
    /// Closed by the service.
    pub service: u64,
    /// Closed by server shutdown.
    pub shutdown: u64,
    /// Closed by application-defined reasons, `custom[0]` counts
    /// [`CloseReason::Custom1`] and so on.
    pub custom: [u64; 8],
    /// Closed without a reported reason, e.g. by the application.
    pub other: u64,
}

/// Backpressure statistics of one direction.
///
/// The connection-time under backpressure is the sum of all ended periods
/// plus the open ones. Each period adds its start to `starts` and its end to
/// `ends`, so the total at time `now` is `ends - starts + active * now`,
/// without any per-connection state. Times are nanoseconds since `base`.
///
/// The fields are updated under a sequence lock, readers retry until they
/// read the fields of one version, without blocking writers. Writers to the
/// same metrics on several threads take turns.
#[derive(Debug, Default)]
struct Backpressure {
    seq: AtomicU64,
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
    /// Connections that keep a read buffer allocated.
    ///
    /// Read buffers are released once their input is consumed, so a
    /// connection is counted while it holds unconsumed input, such as the
    /// start of a frame that has not fully arrived. A snapshot mostly counts
    /// idle connections waiting for the rest of their input, busy ones are
    /// only briefly counted. A buffer a transport holds for a pending read,
    /// as with io-uring, is not counted.
    pub read_buffers_held: usize,
    /// Bytes allocated by the read buffers of
    /// [`read_buffers_held`](Self::read_buffers_held), a whole page for a
    /// pooled buffer.
    ///
    /// Compared with [`read_buffered`](Self::read_buffered) it shows the
    /// memory kept for small leftover input. Frames split off a read buffer
    /// share its allocation and can keep it alive after it is released, that
    /// memory is not counted.
    pub read_buffer_capacity: usize,
    /// Bytes held in write buffers or taken by the transport but not written.
    pub write_buffered: usize,
    /// Bytes read from transports.
    pub bytes_read: u64,
    /// Bytes written to transports.
    pub bytes_written: u64,
    /// Writes started in place because buffered output reached the write
    /// buffer threshold, see `IoConfig::set_write_buf_threshold()`.
    pub eager_writes: u64,
    /// Eager writes that could not write all buffered output, the rest is
    /// left to the write task.
    pub eager_writes_partial: u64,
    /// Writes left to the write task, below the write buffer threshold or with
    /// eager writes disabled. The io-uring transport always uses the write
    /// task.
    pub scheduled_writes: u64,
    /// Closed connections by reason.
    pub closed: CloseStats,
}

impl IoMetrics {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(IoMetrics {
            base: Instant::now(),
            rd: Backpressure::default(),
            wr: Backpressure::default(),
            read_buffered: AtomicUsize::new(0),
            read_buffers_held: AtomicUsize::new(0),
            read_buffer_capacity: AtomicUsize::new(0),
            write_buffered: AtomicUsize::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
            eager_writes: AtomicU64::new(0),
            eager_writes_partial: AtomicU64::new(0),
            scheduled_writes: AtomicU64::new(0),
            closed: Default::default(),
        })
    }

    /// Metrics not in any group, for placeholder connection states.
    pub(crate) fn detached() -> Arc<Self> {
        DETACHED
            .try_with(Clone::clone)
            .unwrap_or_else(|_| IoMetrics::new())
    }

    /// Returns a snapshot of the statistics.
    pub fn snapshot(&self) -> IoMetricsSnapshot {
        let (read_backpressure, read_backpressure_active, read_backpressure_time) =
            self.rd.read(self.base);
        let (write_backpressure, write_backpressure_active, write_backpressure_time) =
            self.wr.read(self.base);
        IoMetricsSnapshot {
            read_backpressure,
            write_backpressure,
            read_backpressure_active,
            write_backpressure_active,
            read_backpressure_time,
            write_backpressure_time,
            read_buffered: self.read_buffered.load(Relaxed),
            read_buffers_held: self.read_buffers_held.load(Relaxed),
            read_buffer_capacity: self.read_buffer_capacity.load(Relaxed),
            write_buffered: self.write_buffered.load(Relaxed),
            bytes_read: self.bytes_read.load(Relaxed),
            bytes_written: self.bytes_written.load(Relaxed),
            eager_writes: self.eager_writes.load(Relaxed),
            eager_writes_partial: self.eager_writes_partial.load(Relaxed),
            scheduled_writes: self.scheduled_writes.load(Relaxed),
            closed: self.close_stats(),
        }
    }

    fn close_stats(&self) -> CloseStats {
        let c: [u64; CLOSE_REASONS] = std::array::from_fn(|i| self.closed[i].load(Relaxed));
        CloseStats {
            total: c.iter().sum(),
            keepalive: c[CloseReason::KeepAlive.index()],
            read_timeout: c[CloseReason::ReadTimeout.index()],
            write_timeout: c[CloseReason::WriteTimeout.index()],
            peer_gone: c[CloseReason::PeerGone.index()],
            io: c[CloseReason::Io.index()],
            decoder: c[CloseReason::Decoder.index()],
            encoder: c[CloseReason::Encoder.index()],
            service: c[CloseReason::Service.index()],
            shutdown: c[CloseReason::Shutdown.index()],
            custom: std::array::from_fn(|i| c[CloseReason::Custom1.index() + i]),
            other: c[CLOSE_OTHER],
        }
    }

    /// Counts a closed connection.
    pub(crate) fn closed(&self, reason: Option<CloseReason>, io_error: bool) {
        let idx = match reason {
            Some(r) => r.index(),
            None if io_error => CloseReason::Io.index(),
            None => CLOSE_OTHER,
        };
        self.closed[idx].fetch_add(1, Relaxed);
    }

    fn now(&self) -> u64 {
        nanos(self.base, ntex_rt::now())
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

    /// Moves a connection's gauges from `self` to `to`, e.g. after the
    /// connection's configuration was replaced. Counters stay.
    pub(crate) fn move_to(
        &self,
        to: &IoMetrics,
        rd_bp: bool,
        wr_bp: bool,
        rd: u32,
        rd_cap: u32,
        wr: u32,
    ) {
        if rd_bp {
            self.rd.off(self.now());
            to.rd.resume(to.now());
        }
        if wr_bp {
            self.wr.off(self.now());
            to.wr.resume(to.now());
        }
        self.update_read_buffered(rd, 0, rd_cap, 0);
        to.update_read_buffered(0, rd, 0, rd_cap);
        self.update_write_buffered(wr, 0);
        to.update_write_buffered(0, wr);
    }

    pub(crate) fn read(&self, n: usize) {
        inc(&self.bytes_read, n as u64);
    }

    pub(crate) fn written(&self, n: usize) {
        inc(&self.bytes_written, n as u64);
    }

    pub(crate) fn eager_write(&self, partial: bool) {
        inc(&self.eager_writes, 1);
        if partial {
            inc(&self.eager_writes_partial, 1);
        }
    }

    pub(crate) fn scheduled_write(&self) {
        inc(&self.scheduled_writes, 1);
    }

    /// Updates the read buffer gauges of a connection, `cap` is the retained
    /// capacity, a connection with capacity holds a read buffer.
    pub(crate) fn update_read_buffered(&self, prev: u32, cur: u32, prev_cap: u32, cap: u32) {
        update(&self.read_buffered, prev, cur);
        update(&self.read_buffer_capacity, prev_cap, cap);
        match (prev_cap > 0, cap > 0) {
            (false, true) => {
                self.read_buffers_held.fetch_add(1, Relaxed);
            }
            (true, false) => {
                self.read_buffers_held.fetch_sub(1, Relaxed);
            }
            _ => (),
        }
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
        self.write(|| {
            inc(&self.count, 1);
            self.start(now);
        });
    }

    // continues a period started on other metrics, not counted again
    fn resume(&self, now: u64) {
        self.write(|| self.start(now));
    }

    fn off(&self, now: u64) {
        self.write(|| {
            inc(&self.ends, now);
            let active = self.active.load(Relaxed);
            self.active.store(active.wrapping_sub(1), Relaxed);
        });
    }

    fn start(&self, now: u64) {
        inc(&self.starts, now);
        let active = self.active.load(Relaxed);
        self.active.store(active.wrapping_add(1), Relaxed);
    }

    /// Updates the fields under the write lock, `seq` is odd while locked.
    fn write(&self, f: impl FnOnce()) {
        let mut seq = self.seq.load(Relaxed);
        loop {
            if seq & 1 == 0 {
                match self
                    .seq
                    .compare_exchange_weak(seq, seq + 1, Acquire, Relaxed)
                {
                    Ok(_) => break,
                    Err(s) => seq = s,
                }
            } else {
                std::hint::spin_loop();
                seq = self.seq.load(Relaxed);
            }
        }
        // field stores are not visible before the odd `seq`
        fence(Release);
        f();
        self.seq.store(seq.wrapping_add(2), Release);
    }

    /// Returns the count, active periods and total time at the current time,
    /// read from one version of the fields.
    fn read(&self, base: Instant) -> (u64, usize, Duration) {
        loop {
            let seq = self.seq.load(Acquire);
            if seq & 1 == 0 {
                let count = self.count.load(Relaxed);
                let active = self.active.load(Relaxed);
                let starts = self.starts.load(Relaxed);
                let ends = self.ends.load(Relaxed);
                fence(Acquire);
                if self.seq.load(Relaxed) == seq {
                    // after the acquire, not before the writer's times
                    let now = nanos(base, Instant::now());
                    let total = ends
                        .wrapping_add((active as u64).wrapping_mul(now))
                        .wrapping_sub(starts);
                    // a writer's cached time may be ahead of `now`
                    let total = if total > i64::MAX as u64 { 0 } else { total };
                    return (count, active, Duration::from_nanos(total));
                }
            }
            std::hint::spin_loop();
        }
    }
}

fn nanos(base: Instant, t: Instant) -> u64 {
    u64::try_from(t.saturating_duration_since(base).as_nanos()).unwrap_or(u64::MAX)
}

// single writer, cheaper than `fetch_add`
#[inline]
fn inc(counter: &AtomicU64, n: u64) {
    counter.store(counter.load(Relaxed).wrapping_add(n), Relaxed);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use ntex_service::cfg::SharedCfg;

    use super::*;

    #[test]
    fn backpressure_time_is_coherent() {
        let m = IoMetrics::new();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (m, stop) = (m.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Relaxed) {
                    m.rd.on(nanos(m.base, Instant::now()));
                    m.rd.off(nanos(m.base, Instant::now()));
                }
            })
        };
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            let s = m.snapshot();
            // a single connection, its time is bounded by the metrics' uptime
            let uptime = m.base.elapsed();
            assert!(
                s.read_backpressure_time <= uptime,
                "{:?} > {uptime:?}",
                s.read_backpressure_time
            );
            assert!(s.read_backpressure_active <= 1);
        }
        stop.store(true, Relaxed);
        writer.join().unwrap();
        let s = m.snapshot();
        assert_eq!(s.read_backpressure_active, 0);
        assert!(s.read_backpressure > 0);
    }

    #[test]
    fn unregistered_configs_are_not_cached() {
        let group = IoMetricsGroup::new();
        IoMetricsGroup::set_current(group.clone());
        let srv = SharedCfg::new("SRV").build();
        group.register(&srv.get::<IoConfig>());

        for _ in 0..1000 {
            let cfg = SharedCfg::new("CLI").build();
            assert!(Arc::ptr_eq(&cfg.get::<IoConfig>().metrics(), group.other()));
        }
        let srv_metrics = srv.get::<IoConfig>().metrics();
        assert!(Arc::ptr_eq(
            &srv_metrics,
            &group.get(&srv.get::<IoConfig>())
        ));
        assert!(!Arc::ptr_eq(&srv_metrics, group.other()));
        let cached = CURRENT.with(|c| c.borrow().as_ref().unwrap().cache.len());
        assert_eq!(cached, 1);
    }
}
