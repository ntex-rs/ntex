//! Worker pool and worker integration tests.
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::{io, time::Duration};

use ntex::time::{Millis, sleep};
use ntex_server::{
    Affinity, LoadBalancing, Server, ServerConfiguration, ServerStatus, Worker, WorkerPool,
    WorkerStatus,
};
use ntex_service::{Ctx, Service};

#[derive(Clone, Default)]
struct Counters {
    created: Arc<AtomicUsize>,
    create_failures: Arc<AtomicUsize>,
    ready_failures: Arc<AtomicUsize>,
    pause: Arc<AtomicUsize>,
    resume: Arc<AtomicUsize>,
    terminate: Arc<AtomicUsize>,
    stop: Arc<AtomicUsize>,
    shutdown: Arc<AtomicUsize>,
    items: Arc<Mutex<Vec<usize>>>,
    threads: Arc<Mutex<Vec<(usize, String)>>>,
}

impl Counters {
    fn items(&self) -> Vec<usize> {
        self.items.lock().unwrap().clone()
    }

    /// Items with the name of the worker thread that processed them.
    fn threads(&self) -> Vec<(usize, String)> {
        self.threads.lock().unwrap().clone()
    }
}

/// Test pool configuration.
#[derive(Clone)]
struct Cfg {
    c: Counters,
    /// Number of initial `create` calls that fail.
    fail_create: usize,
    /// Delay before the service is created.
    create_delay: Millis,
    /// Delay of the service shutdown.
    shutdown_delay: Millis,
    /// Number of initial readiness checks that fail.
    fail_ready: usize,
    /// Readiness checks never complete.
    pending_ready: bool,
    /// Panic when processing this item, after blocking on it.
    panic_on: Option<usize>,
    /// Block the worker thread when processing this item.
    block_on: Option<(usize, Duration)>,
    /// The incoming CPU of items is `item % 1000`.
    incoming_cpu: bool,
    items: Option<mpsc::Sender<usize>>,
}

impl Cfg {
    fn new(c: &Counters) -> Self {
        Cfg {
            c: c.clone(),
            fail_create: 0,
            create_delay: Millis::ZERO,
            shutdown_delay: Millis::ZERO,
            fail_ready: 0,
            pending_ready: false,
            panic_on: None,
            block_on: None,
            incoming_cpu: false,
            items: None,
        }
    }
}

struct Srv {
    cfg: Cfg,
}

impl Srv {
    fn block(&self, item: usize) {
        if let Some((block, dur)) = self.cfg.block_on
            && block == item
        {
            std::thread::sleep(dur);
        }
    }
}

impl Service<(), usize> for Srv {
    type Res = ();
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        if self.cfg.pending_ready {
            std::future::pending::<()>().await;
        }
        if self.cfg.c.ready_failures.load(Relaxed) < self.cfg.fail_ready {
            self.cfg.c.ready_failures.fetch_add(1, Relaxed);
            Err(())
        } else {
            Ok(())
        }
    }

    async fn call(&self, item: usize, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        if self.cfg.panic_on == Some(item) {
            self.block(item);
            panic!("item {item}");
        }
        self.cfg.c.items.lock().unwrap().push(item);
        let name = std::thread::current()
            .name()
            .unwrap_or_default()
            .to_string();
        self.cfg.c.threads.lock().unwrap().push((item, name));
        self.block(item);
        if let Some(ref tx) = self.cfg.items {
            let _ = tx.send(item);
        }
        Ok(())
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        if !self.cfg.shutdown_delay.is_zero() {
            sleep(self.cfg.shutdown_delay).await;
        }
        self.cfg.c.shutdown.fetch_add(1, Relaxed);
    }
}

impl ServerConfiguration for Cfg {
    type Item = usize;
    type Service = Srv;

    async fn create(&self) -> io::Result<Srv> {
        if !self.create_delay.is_zero() {
            sleep(self.create_delay).await;
        }
        if self.c.create_failures.load(Relaxed) < self.fail_create {
            self.c.create_failures.fetch_add(1, Relaxed);
            return Err(io::Error::other("create failed"));
        }
        self.c.created.fetch_add(1, Relaxed);
        Ok(Srv { cfg: self.clone() })
    }

    fn pause(&self) {
        self.c.pause.fetch_add(1, Relaxed);
    }

    fn resume(&self) {
        self.c.resume.fetch_add(1, Relaxed);
    }

    fn terminate(&self) {
        self.c.terminate.fetch_add(1, Relaxed);
    }

    async fn stop(&self) {
        self.c.stop.fetch_add(1, Relaxed);
    }

    fn incoming_cpu(&self, item: &usize) -> Option<usize> {
        self.incoming_cpu.then_some(item % 1000)
    }
}

fn pool() -> WorkerPool {
    WorkerPool::new().name("pool").disable_signals()
}

/// Submits an item, retrying while the server has no available worker.
async fn process(srv: &Server<usize>, item: usize) {
    for _ in 0..500 {
        if srv.process(item).is_ok() {
            return;
        }
        sleep(Millis(10)).await;
    }
    panic!("server does not accept items");
}

/// Waits until the server has the status.
async fn wait_status(srv: &Server<usize>, status: ServerStatus) {
    let wait = async {
        loop {
            let changed = srv.status_changed();
            if srv.status() == status {
                return;
            }
            changed.await;
        }
    };
    ntex::time::timeout(Millis(5000), wait)
        .await
        .unwrap_or_else(|()| panic!("status is not {status:?}: {:?}", srv.status()));
}

async fn wait_for(f: impl Fn() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        sleep(Millis(10)).await;
    }
    panic!("condition is not met");
}

/// STATEMENT: A worker pool processes items across its workers, rejects items while paused
/// or stopped, and runs the configuration's lifecycle hooks once per event.
///
/// HOW:
/// - Process 10 items on 2 workers; expect all items, 2 services and a resume.
/// - Pause and expect `process` to return the item; resume and process one more.
/// - Check worker metrics count 11 items with no restarts, connections or listeners.
/// - Stop gracefully; expect the awaited server to finish, one stop, 2 shutdowns, one
///   terminate, and later `process` calls to return the item.
#[ntex::test]
async fn pool_process_items() {
    let c = Counters::default();
    let srv = pool()
        .workers(2)
        .graceful_shutdown_timeout(Millis(500))
        .run(Cfg::new(&c));

    for i in 0..10 {
        process(&srv, i).await;
    }
    wait_for(|| c.items().len() == 10).await;
    wait_for(|| c.created.load(Relaxed) == 2).await;
    assert!(c.resume.load(Relaxed) >= 1);

    // a paused server rejects items
    srv.pause().await;
    assert_eq!(srv.process(100), Err(100));
    assert!(c.pause.load(Relaxed) >= 1);
    srv.resume().await;
    process(&srv, 11).await;
    wait_for(|| c.items().len() == 11).await;

    let m = srv.metrics();
    assert_eq!(m.workers.len(), 2);
    assert_eq!(m.workers.iter().map(|w| w.items).sum::<u64>(), 11);
    assert!(m.workers.iter().all(|w| w.restarts == 0));
    assert!(m.workers.iter().all(|w| w.active_connections == 0));
    assert!(m.listeners.is_empty());

    let srv2 = srv.clone();
    let stopped = ntex::rt::spawn(srv2);
    sleep(Millis(50)).await;
    srv.stop(true).await;
    assert!(stopped.await.unwrap().is_ok());
    assert_eq!(c.stop.load(Relaxed), 1);
    assert_eq!(c.shutdown.load(Relaxed), 2);
    wait_for(|| c.terminate.load(Relaxed) == 1).await;

    // the server is gone
    assert_eq!(srv.process(12), Err(12));
    srv.stop(true).await;
    srv.pause().await;
    srv.resume().await;
    assert!(srv.clone().await.is_ok());
}

/// STATEMENT: Items are sent to workers on the submitting thread, so processing does not
/// depend on the server's command loop.
///
/// HOW:
/// - Process one item and receive it from the worker.
/// - Block the test thread on a helper thread that submits items 1 to 10.
/// - Expect every submit to succeed and all 10 items to reach the workers.
#[ntex::test]
async fn pool_dispatch_on_submitting_thread() {
    let c = Counters::default();
    let (tx, rx) = mpsc::channel();
    let mut cfg = Cfg::new(&c);
    cfg.items = Some(tx);
    let srv = pool().workers(2).run(cfg);
    process(&srv, 0).await;
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(0));

    // blocks this thread
    let srv2 = srv.clone();
    std::thread::spawn(move || {
        for i in 1..=10 {
            assert_eq!(srv2.process(i), Ok(()));
        }
    })
    .join()
    .unwrap();
    let mut items: Vec<_> = (1..=10)
        .map(|_| rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .collect();
    items.sort_unstable();
    assert_eq!(items, (1..=10).collect::<Vec<_>>());

    srv.stop(false).await;
}

/// STATEMENT: Least-loaded balancing stops sending items to a worker that is blocked.
///
/// HOW:
/// - Start 2 workers whose service blocks for 1 s on item 0, and wait for latency samples.
/// - Process item 0 to block one worker, then process items 1 to 20.
/// - Expect none of the later items to run on the blocked worker's thread.
#[ntex::test]
async fn pool_least_loaded_skips_blocked_worker() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.block_on = Some((0, Duration::from_millis(1000)));
    // least loaded balancing is the default for items without incoming CPU
    let srv = pool().workers(2).run(cfg);
    wait_for(|| c.created.load(Relaxed) == 2).await;
    // tokio does not report runtime turns, a blocked worker is detected
    // by its missing latency samples, wait for the first sample
    sleep(Millis(250)).await;

    // blocks one worker
    process(&srv, 0).await;
    wait_for(|| c.threads().len() == 1).await;
    let blocked = c.threads()[0].1.clone();
    sleep(Millis(300)).await;

    for i in 1..=20 {
        process(&srv, i).await;
    }
    wait_for(|| c.threads().len() == 21).await;
    let threads = c.threads();
    assert!(
        threads[1..].iter().all(|(_, name)| *name != blocked),
        "{threads:?}"
    );

    srv.stop(false).await;
}

/// STATEMENT: A worker that was blocked receives items again as soon as it
/// is no longer blocked, its blocked time does not count as load.
///
/// HOW:
/// - Block one of two idle workers for 600 ms with the first item, wait until
///   the item completed and 150 ms more for the next latency sample.
/// - Submit 20 items 10 ms apart and confirm the recovered worker processes
///   some of them; counting the blocked time would make it lose every
///   comparison for about 500 ms.
#[ntex::test]
async fn pool_least_loaded_uses_recovered_worker() {
    let c = Counters::default();
    let (tx, rx) = mpsc::channel();
    let mut cfg = Cfg::new(&c);
    cfg.block_on = Some((0, Duration::from_millis(600)));
    cfg.items = Some(tx);
    let srv = pool().workers(2).run(cfg);
    wait_for(|| c.created.load(Relaxed) == 2).await;
    // wait for the first latency sample, see above
    sleep(Millis(250)).await;

    process(&srv, 0).await;
    wait_for(|| c.threads().len() == 1).await;
    let blocked = c.threads()[0].1.clone();
    wait_for(|| rx.try_recv() == Ok(0)).await;
    // the next latency sample ends the block
    sleep(Millis(150)).await;

    for i in 1..=20 {
        process(&srv, i).await;
        sleep(Millis(10)).await;
    }
    wait_for(|| c.threads().len() == 21).await;
    let threads = c.threads();
    assert!(
        threads[1..].iter().any(|(_, name)| *name == blocked),
        "{threads:?}"
    );

    srv.stop(false).await;
}

/// STATEMENT: Items queued at a blocked worker are rescheduled to another worker.
///
/// HOW:
/// - Start 2 round-robin workers whose service blocks for 2 s on item 0.
/// - Queue items 0 to 10 on both workers before the block is detected.
/// - Expect only item 0 on the blocked thread, 1 to 5 rescheduled items and 11 processed.
#[ntex::test]
async fn pool_reschedules_blocked_worker_queue() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.block_on = Some((0, Duration::from_millis(2000)));
    let srv = pool()
        .workers(2)
        .load_balancing(LoadBalancing::RoundRobin)
        .run(cfg);
    wait_for(|| c.created.load(Relaxed) == 2).await;
    // wait for the first latency sample, see above
    sleep(Millis(250)).await;

    // the items are queued at both workers before the block is detected
    for i in 0..=10 {
        process(&srv, i).await;
    }
    wait_for(|| c.threads().len() == 11).await;
    let threads = c.threads();
    let blocked = threads.iter().find(|(i, _)| *i == 0).unwrap().1.clone();
    assert!(
        threads.iter().all(|(i, name)| *i == 0 || *name != blocked),
        "{threads:?}"
    );
    let m = srv.metrics();
    assert!(m.rescheduled > 0 && m.rescheduled <= 5, "{}", m.rescheduled);
    assert_eq!(m.workers.iter().map(|w| w.items).sum::<u64>(), 11);

    srv.stop(false).await;
}

/// STATEMENT: Items queued at a failed worker are sent to its replacement.
///
/// HOW:
/// - Start one worker that blocks and then panics on item 0, and queue items 0 to 5.
/// - Expect items 1 to 5 to be processed in order by the restarted worker.
/// - Check metrics report 5 rescheduled items and one restart.
#[ntex::test]
async fn pool_reschedules_failed_worker_queue() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.panic_on = Some(0);
    cfg.block_on = Some((0, Duration::from_millis(200)));
    let srv = pool().workers(1).run(cfg);

    for i in 0..=5 {
        process(&srv, i).await;
    }
    wait_for(|| c.items().len() == 5).await;
    assert_eq!(c.items(), [1, 2, 3, 4, 5]);
    let m = srv.metrics();
    assert_eq!(m.rescheduled, 5);
    assert_eq!(m.workers[0].restarts, 1);

    srv.stop(false).await;
}

/// Returns the indexes of the logical CPUs the process may use.
fn allowed_cpus() -> Vec<usize> {
    sys_allowed_cpus().unwrap_or_else(|| {
        (0..std::thread::available_parallelism().map_or(2, |n| n.get())).collect()
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sys_allowed_cpus() -> Option<Vec<usize>> {
    // SAFETY: cpu_set_t is plain data, the size matches the set
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &raw mut set) != 0 {
            return None;
        }
        // `CPU_SETSIZE` is a `c_int` on Linux and a `usize` on Android
        let size = u32::try_from(libc::CPU_SETSIZE).unwrap() as usize;
        Some((0..size).filter(|&n| libc::CPU_ISSET(n, &set)).collect())
    }
}

#[cfg(windows)]
fn sys_allowed_cpus() -> Option<Vec<usize>> {
    use windows_sys::Win32::System::Threading::{
        GetActiveProcessorCount, GetCurrentProcess, GetProcessAffinityMask, GetProcessGroupAffinity,
    };

    // SAFETY: plain process affinity queries
    unsafe {
        let process = GetCurrentProcess();
        let (mut mask, mut system) = (0, 0);
        let (mut count, mut group) = (1u16, 0u16);
        // zero mask if the process runs in several processor groups
        if GetProcessAffinityMask(process, &raw mut mask, &raw mut system) == 0
            || mask == 0
            || GetProcessGroupAffinity(process, &raw mut count, &raw mut group) == 0
        {
            return None;
        }
        // CPUs are numbered across processor groups
        let base: u32 = (0..group).map(|g| GetActiveProcessorCount(g)).sum();
        Some(
            (0..usize::BITS)
                .filter(|n| mask & (1 << n) != 0)
                .map(|n| (base + n) as usize)
                .collect(),
        )
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
fn sys_allowed_cpus() -> Option<Vec<usize>> {
    None
}

/// STATEMENT: With incoming-CPU balancing, items go to the worker placed on their incoming
/// CPU; least-loaded balancing does not use it.
///
/// HOW:
/// - For `IncomingCpu` and `LeastLoaded`, start 2 CPU-pinned workers and process 3 rounds
///   of one item per allowed CPU.
/// - For `LeastLoaded` or platforms without incoming CPU, expect no hits or misses.
/// - Otherwise expect every item counted, each CPU always hit or always missed, and hit
///   items handled by a single worker.
#[ntex::test]
async fn pool_incoming_cpu() {
    let cpus = allowed_cpus();
    for balancing in [LoadBalancing::IncomingCpu, LoadBalancing::LeastLoaded] {
        let c = Counters::default();
        let mut cfg = Cfg::new(&c);
        cfg.incoming_cpu = true;
        let srv = pool()
            .workers(2)
            .affinity(Affinity::Cpu)
            .load_balancing(balancing)
            .run(cfg);
        wait_for(|| c.created.load(Relaxed) == 2).await;

        let mut total = 0;
        for round in 0..3 {
            for &cpu in &cpus {
                // idle workers, the worker on the CPU is not busier
                process(&srv, round * 1000 + cpu).await;
                total += 1;
                wait_for(|| c.items().len() == total).await;
            }
        }

        let m = srv.metrics();
        let (hits, misses) = (m.incoming_cpu_hits, m.incoming_cpu_misses);
        if balancing == LoadBalancing::LeastLoaded
            || cfg!(not(any(
                windows,
                target_os = "linux",
                target_os = "android"
            )))
        {
            assert_eq!((hits, misses), (0, 0));
        } else {
            assert_eq!(hits + misses, total as u64);
            // each CPU is always hit or always missed
            assert!(hits >= 3 && hits % 3 == 0, "{hits}");
            let threads = c.threads();
            let mut local = 0;
            for &cpu in &cpus {
                let mut names: Vec<_> = threads
                    .iter()
                    .filter(|(item, _)| item % 1000 == cpu)
                    .map(|(_, name)| name)
                    .collect();
                names.dedup();
                if names.len() == 1 {
                    local += 3;
                }
            }
            // with one CPU, both workers are placed on it
            assert!(
                cpus.len() < 2 || local >= hits,
                "{local} {hits} {threads:?}"
            );
        }
        srv.stop(false).await;
    }
}

/// STATEMENT: Items queued while no worker is available are balanced by their incoming
/// CPU once the workers restart.
///
/// HOW:
/// - Start 2 CPU-pinned workers, then, from a helper thread that blocks the command loop,
///   panic both workers and submit one item per allowed CPU.
/// - Expect every item to be processed and each worker restarted once.
/// - On platforms with incoming CPU, expect the backlog items counted with some hits;
///   otherwise expect no hits or misses.
#[ntex::test]
async fn pool_incoming_cpu_backlog() {
    const PANIC: usize = 1_000_000;
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.incoming_cpu = true;
    cfg.panic_on = Some(PANIC);
    let srv = pool().workers(2).affinity(Affinity::Cpu).run(cfg);
    wait_for(|| c.created.load(Relaxed) == 2).await;
    wait_status(&srv, ServerStatus::Ready).await;

    // blocks the command loop, the failed workers are not restarted and the
    // server stays ready, the items that find no worker wait in the backlog
    let srv2 = srv.clone();
    let total = std::thread::spawn(move || {
        for _ in 0..2 {
            assert_eq!(srv2.process(PANIC), Ok(()));
            std::thread::sleep(Duration::from_millis(200));
        }
        allowed_cpus()
            .into_iter()
            .filter(|&cpu| srv2.process(cpu).is_ok())
            .count()
    })
    .join()
    .unwrap();
    assert_eq!(total, allowed_cpus().len());
    let m = srv.metrics();
    let (hits0, misses0) = (m.incoming_cpu_hits, m.incoming_cpu_misses);
    wait_for(|| c.items().len() == total).await;

    let m = srv.metrics();
    assert!(m.workers.iter().all(|w| w.restarts == 1));
    let (hits, misses) = (m.incoming_cpu_hits - hits0, m.incoming_cpu_misses - misses0);
    if cfg!(any(windows, target_os = "linux", target_os = "android")) {
        assert_eq!(hits + misses, total as u64);
        assert!(hits > 0);
    } else {
        assert_eq!((hits, misses), (0, 0));
    }
    srv.stop(false).await;
}

/// STATEMENT: Worker metrics report the runtime's load and a blocked worker as stalled.
///
/// HOW:
/// - Process an item; expect busy time, latency samples and no stalls, plus poll counts
///   on runtimes that report turns.
/// - Process an item that blocks the worker for 1.5 s.
/// - Expect the latency to report a stall, then one finished stall with a non-zero duration.
#[ntex::test]
async fn pool_runtime_metrics() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.block_on = Some((1, Duration::from_millis(1500)));
    let srv = pool().workers(1).run(cfg);
    let rt = || srv.metrics().workers[0].runtime.clone();

    process(&srv, 0).await;
    wait_for(|| c.items().len() == 1).await;
    wait_for(|| rt().latency.is_some_and(|l| l.histogram.count() > 0)).await;
    let m = rt();
    assert!(m.busy_duration.is_some_and(|d| !d.is_zero()));
    assert_eq!(m.latency.unwrap().stalls, 0);
    // tokio does not report runtime turns
    let turns = m.polls.is_some();
    if turns {
        assert!(m.polls.is_some_and(|n| n > 0));
        assert!(m.poll_histogram.is_some_and(|h| h.count() > 0));
    }

    // a blocked worker is reported as stalled
    process(&srv, 1).await;
    wait_for(|| c.items().len() == 2).await;
    if turns {
        assert!(rt().current_turn.is_some());
    }
    wait_for(|| rt().latency.is_some_and(|l| l.stalled)).await;
    wait_for(|| rt().latency.is_some_and(|l| !l.stalled && l.stalls == 1)).await;
    assert!(rt().latency.unwrap().stall_duration > Duration::ZERO);

    srv.stop(false).await;
}

/// STATEMENT: Every affinity mode starts all workers, even with more workers than CPUs.
///
/// HOW:
/// - For `Disabled`, `Cpu`, `Core` and `Soft`, start one more worker than available CPUs.
/// - Process one item per worker; expect all items processed and all services created.
#[ntex::test]
async fn pool_affinity() {
    let num = std::thread::available_parallelism().map_or(2, |n| n.get()) + 1;
    for mode in [
        Affinity::Disabled,
        Affinity::Cpu,
        Affinity::Core,
        Affinity::Soft,
    ] {
        let c = Counters::default();
        let srv = pool().workers(num).affinity(mode).run(Cfg::new(&c));
        for i in 0..num {
            process(&srv, i).await;
        }
        wait_for(|| c.items().len() == num).await;
        wait_for(|| c.created.load(Relaxed) == num).await;
        srv.stop(false).await;
    }
}

/// STATEMENT: A non-graceful stop runs the stop hook and shuts the workers down with the
/// default timeout.
///
/// HOW:
/// - Start one worker with a 50 ms shutdown delay and process an item.
/// - Stop with `graceful = false`; expect one stop, then one service shutdown.
#[ntex::test]
async fn pool_non_graceful_stop() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.shutdown_delay = Millis(50);
    let srv = pool().workers(1).run(cfg);
    process(&srv, 1).await;

    srv.stop(false).await;
    assert_eq!(c.stop.load(Relaxed), 1);
    // workers shut down with the default timeout
    wait_for(|| c.shutdown.load(Relaxed) == 1).await;
}

/// STATEMENT: A zero graceful timeout still lets a graceful stop shut the workers down.
///
/// HOW:
/// - Start one worker with `graceful_shutdown_timeout(Millis::ZERO)` and process an item.
/// - Stop gracefully and expect one service shutdown.
#[ntex::test]
async fn pool_zero_shutdown_timeout() {
    let c = Counters::default();
    let srv = pool()
        .workers(1)
        .graceful_shutdown_timeout(Millis::ZERO)
        .run(Cfg::new(&c));
    process(&srv, 1).await;
    srv.stop(true).await;
    wait_for(|| c.shutdown.load(Relaxed) == 1).await;
}

/// STATEMENT: Workers that exceed the graceful timeout are abandoned.
///
/// HOW:
/// - Start one worker whose shutdown takes 5 s, with a 100 ms graceful timeout.
/// - Stop gracefully; expect the stop to finish within 3 s with no completed shutdown.
#[ntex::test]
async fn pool_graceful_timeout_expires() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.shutdown_delay = Millis(5000);
    let srv = pool()
        .workers(1)
        .graceful_shutdown()
        .graceful_shutdown_timeout(Millis(100))
        .run(cfg);
    process(&srv, 1).await;

    let start = std::time::Instant::now();
    srv.stop(true).await;
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(c.shutdown.load(Relaxed), 0);
}

/// STATEMENT: Workers whose service is not ready are stopped with the server.
///
/// HOW:
/// - For non-graceful and graceful stops, start one worker whose service stays not ready.
/// - Wait until metrics report it not ready, stop, and expect one service shutdown.
#[ntex::test]
async fn pool_stop_unavailable_worker() {
    for graceful in [false, true] {
        let c = Counters::default();
        let mut cfg = Cfg::new(&c);
        cfg.pending_ready = true;
        let srv = pool()
            .workers(1)
            .graceful_shutdown_timeout(Millis(500))
            .run(cfg);
        wait_for(|| srv.metrics().workers[0].not_ready == 1).await;

        srv.stop(graceful).await;
        wait_for(|| c.shutdown.load(Relaxed) == 1).await;
    }
}

/// STATEMENT: Workers still creating their service are stopped with the server, and the
/// service is never created.
///
/// HOW:
/// - Start one worker whose service takes 300 ms to create, and stop after 50 ms.
/// - Wait past the creation delay and expect no service created.
#[ntex::test]
async fn pool_stop_initializing_worker() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.create_delay = Millis(300);
    let srv = pool().workers(1).run(cfg);
    sleep(Millis(50)).await;

    srv.stop(true).await;
    sleep(Millis(500)).await;
    assert_eq!(c.created.load(Relaxed), 0);
}

/// STATEMENT: A worker whose service fails to start is restarted.
///
/// HOW:
/// - Start one worker whose first service creation fails, and process an item.
/// - Expect the item processed, one failure, one created service, and one restart and
///   one item in the metrics.
#[ntex::test]
async fn pool_restarts_failed_worker() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.fail_create = 1;
    let srv = pool().workers(1).run(cfg);

    process(&srv, 1).await;
    wait_for(|| c.items().len() == 1).await;
    assert_eq!(c.create_failures.load(Relaxed), 1);
    assert_eq!(c.created.load(Relaxed), 1);
    let m = srv.metrics();
    assert_eq!(m.workers[0].restarts, 1);
    assert_eq!(m.workers[0].items, 1);
    srv.stop(false).await;
}

/// STATEMENT: With `stop_on_panic`, a failed worker stops the server.
///
/// HOW:
/// - Start one worker whose service creation fails, with `stop_on_panic`.
/// - Expect the awaited server to finish within 5 s, one stop and no created service.
#[ntex::test]
async fn pool_stop_on_failure() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.fail_create = 1;
    let srv = pool()
        .workers(1)
        .stop_on_panic()
        .graceful_shutdown()
        .run(cfg);

    ntex::time::timeout(Millis(5000), srv)
        .await
        .expect("server is not stopped")
        .unwrap();
    assert_eq!(c.stop.load(Relaxed), 1);
    assert_eq!(c.created.load(Relaxed), 0);
}

/// STATEMENT: The server status follows the workers, pause, resume and stop, and items are
/// accepted only while it is `Ready`.
///
/// HOW:
/// - Expect `Starting` then `Ready` with one resume; pause and resume and expect `Paused`
///   and `Ready` with the matching hooks.
/// - Panic the only worker; expect `Unavailable` with a pause, then `Ready` with a resume.
/// - Stop; expect `Stopping` then `Stopped`, one shutdown and no extra pause.
/// - In every non-ready state, expect `process` to return the item.
#[ntex::test]
async fn pool_status() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.create_delay = Millis(100);
    cfg.shutdown_delay = Millis(200);
    cfg.panic_on = Some(1);
    let srv = pool()
        .workers(1)
        .graceful_shutdown_timeout(Millis(1000))
        .run(cfg);
    assert_eq!(srv.status(), ServerStatus::Starting);
    assert_eq!(srv.process(0), Err(0));
    assert_eq!(srv.status_changed().await, ServerStatus::Ready);
    assert_eq!(c.resume.load(Relaxed), 1);

    srv.pause().await;
    assert_eq!(srv.status(), ServerStatus::Paused);
    assert_eq!(srv.process(0), Err(0));
    assert_eq!(c.pause.load(Relaxed), 1);
    srv.resume().await;
    assert_eq!(srv.status(), ServerStatus::Ready);
    assert_eq!(c.resume.load(Relaxed), 2);

    // the failed worker is restarted
    let changed = srv.status_changed();
    assert_eq!(srv.process(1), Ok(()));
    assert_eq!(changed.await, ServerStatus::Unavailable);
    assert_eq!(srv.process(0), Err(0));
    assert_eq!(c.pause.load(Relaxed), 2);
    assert_eq!(srv.status_changed().await, ServerStatus::Ready);
    assert_eq!(c.resume.load(Relaxed), 3);

    let changed = srv.status_changed();
    let stop = srv.stop(true);
    assert_eq!(changed.await, ServerStatus::Stopping);
    assert_eq!(srv.process(0), Err(0));
    assert_eq!(srv.status_changed().await, ServerStatus::Stopped);
    stop.await;
    assert_eq!(srv.status(), ServerStatus::Stopped);
    assert_eq!(srv.status_changed().await, ServerStatus::Stopped);
    // the configuration is stopped, not paused
    assert_eq!(c.pause.load(Relaxed), 2);
    assert_eq!(c.shutdown.load(Relaxed), 1);
}

/// STATEMENT: The signal task does not keep a stopped server alive.
///
/// HOW:
/// - Start a pool with signals enabled, wait until ready, stop it and drop the handle.
/// - Expect the configuration's shared counter to be released within 2 s.
#[ntex::test]
async fn pool_signals_release_stopped_server() {
    let c = Counters::default();
    let srv = WorkerPool::new()
        .name("pool")
        .workers(1)
        .affinity(Affinity::Disabled)
        .run(Cfg::new(&c));
    wait_status(&srv, ServerStatus::Ready).await;
    srv.stop(true).await;
    drop(srv);

    let mut released = false;
    for _ in 0..200 {
        if Arc::strong_count(&c.created) == 1 {
            released = true;
            break;
        }
        sleep(Millis(10)).await;
    }
    ntex::rt::System::current().disable_signals();
    assert!(released, "the stopped server's factory is still alive");
}

/// STATEMENT: A paused server stays paused while a failed worker is restarted.
///
/// HOW:
/// - Panic the only worker and pause the server; wait for the service to be re-created.
/// - Expect the status to stay `Paused`, no extra resume and items to be rejected.
/// - Resume and expect `Ready` with a second resume.
#[ntex::test]
async fn pool_pause_survives_worker_restart() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.panic_on = Some(1);
    let srv = pool().workers(1).run(cfg);
    wait_status(&srv, ServerStatus::Ready).await;
    assert_eq!(c.resume.load(Relaxed), 1);

    assert_eq!(srv.process(1), Ok(()));
    srv.pause().await;
    assert_eq!(srv.status(), ServerStatus::Paused);

    for _ in 0..200 {
        if c.created.load(Relaxed) == 2 {
            break;
        }
        sleep(Millis(10)).await;
    }
    assert_eq!(c.created.load(Relaxed), 2, "the worker is not restarted");
    sleep(Millis(100)).await;
    assert_eq!(srv.status(), ServerStatus::Paused);
    assert_eq!(c.resume.load(Relaxed), 1);
    assert_eq!(srv.process(2), Err(2));

    srv.resume().await;
    assert_eq!(srv.status(), ServerStatus::Ready);
    assert_eq!(c.resume.load(Relaxed), 2);
    srv.stop(true).await;
}

/// STATEMENT: A paused server stays paused when workers become available, and resuming a
/// server without available workers does not make it ready.
///
/// HOW:
/// - Start 2 workers with a 300 ms creation delay; pause and resume, expect `Starting`.
/// - Pause again and wait for both services; expect `Paused`, rejected items, no resume.
/// - Resume; expect `Ready`, one resume, the item processed and no pause hook.
#[ntex::test]
async fn pool_pause_while_starting() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.create_delay = Millis(300);
    let srv = pool().workers(2).run(cfg);

    srv.pause().await;
    assert_eq!(srv.status(), ServerStatus::Paused);
    srv.resume().await;
    assert_eq!(srv.status(), ServerStatus::Starting);
    assert_eq!(srv.process(0), Err(0));
    srv.pause().await;

    wait_for(|| c.created.load(Relaxed) == 2).await;
    sleep(Millis(100)).await;
    assert_eq!(srv.status(), ServerStatus::Paused);
    assert_eq!(srv.process(0), Err(0));
    assert_eq!(c.resume.load(Relaxed), 0);

    srv.resume().await;
    assert_eq!(srv.status(), ServerStatus::Ready);
    assert_eq!(c.resume.load(Relaxed), 1);
    process(&srv, 0).await;
    wait_for(|| c.items() == [0]).await;
    srv.stop(false).await;
    assert_eq!(c.pause.load(Relaxed), 0);
}

/// STATEMENT: A worker that panics is restarted and keeps processing items.
///
/// HOW:
/// - Start one worker that panics on item 1 and process item 1.
/// - Expect the service to be created again and item 2 to be processed.
/// - Check metrics report one restart and 2 items.
#[ntex::test]
async fn pool_worker_panic_restarts_worker() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.panic_on = Some(1);
    let srv = pool().workers(1).run(cfg);

    process(&srv, 1).await;
    wait_for(|| c.created.load(Relaxed) == 2).await;
    process(&srv, 2).await;
    wait_for(|| c.items().contains(&2)).await;
    let m = srv.metrics();
    assert_eq!(m.workers[0].restarts, 1);
    assert_eq!(m.workers[0].items, 2);
    srv.stop(false).await;
}

/// STATEMENT: A failed readiness check re-creates the worker's service.
///
/// HOW:
/// - Start one worker whose first readiness check fails, and process an item.
/// - Expect the item processed, 2 created services and the failed one shut down.
#[ntex::test]
async fn pool_ready_failure_recreates_service() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.fail_ready = 1;
    let srv = pool().workers(1).run(cfg);

    process(&srv, 1).await;
    wait_for(|| c.items().len() == 1).await;
    assert_eq!(c.created.load(Relaxed), 2);
    // the failed service has been shut down
    wait_for(|| c.shutdown.load(Relaxed) == 1).await;
    srv.stop(false).await;
}

/// STATEMENT: A worker that re-creates its service after a failed readiness
/// check stops without waiting for the new service.
///
/// HOW:
/// - Start a worker whose service creation takes 300 ms and whose first
///   readiness check fails.
/// - Once the readiness check has failed, stop the worker while the service
///   is re-created.
/// - Check the stop completes before the creation would and no second
///   service is created.
#[ntex::test]
async fn worker_stop_while_recreating_service() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.create_delay = Millis(300);
    cfg.fail_ready = 1;

    let wrk: Worker<usize> = Worker::start("wrk:recreate".to_string(), cfg, None);
    wait_for(|| c.ready_failures.load(Relaxed) == 1).await;
    assert_eq!(c.created.load(Relaxed), 1);

    let res = ntex::time::timeout(Millis(200), wrk.stop(Millis(1000))).await;
    assert_eq!(res, Ok(true));
    sleep(Millis(400)).await;
    assert_eq!(c.created.load(Relaxed), 1);
}

/// STATEMENT: With `stop_runtime`, stopping the server also stops the current system.
///
/// HOW:
/// - Run a system that starts a `stop_runtime` pool, processes an item and stops it.
/// - Expect `System::run` to return `Ok` with one stop and the item processed.
#[test]
fn pool_stop_runtime() {
    let c = Counters::default();
    let c2 = c.clone();
    let sys = ntex::rt::System::new("stop-runtime", ntex::rt::DefaultRuntime);
    let res = sys.run(move || {
        let srv = pool()
            .workers(1)
            .affinity(Affinity::Cpu)
            .stop_runtime()
            .run(Cfg::new(&c2));
        ntex::rt::spawn(async move {
            process(&srv, 1).await;
            wait_for(|| c2.items().len() == 1).await;
            srv.stop(true).await;
        });
        Ok(())
    });
    assert!(res.is_ok());
    assert_eq!(c.stop.load(Relaxed), 1);
    assert_eq!(c.items(), vec![1]);
}

/// STATEMENT: `WorkerPool` `Debug` output shows the configured name and worker count.
///
/// HOW:
/// - Configure a pool named `test` with 3 workers.
/// - Format it and expect the output to contain `"test"` and `num: 3`.
#[test]
fn pool_builder() {
    let pool = WorkerPool::default()
        .name("test")
        .workers(3)
        .affinity(Affinity::Disabled)
        .graceful_shutdown();
    let s = format!("{pool:?}");
    assert!(s.contains("\"test\"") && s.contains("num: 3"), "{s}");
}

/// STATEMENT: A `Worker` reports its name and status, delivers items, compares and hashes
/// by identity, and stops its service.
///
/// HOW:
/// - Start a worker; expect its name, `Available` status and the name in `Debug`.
/// - Send an item and receive it; compare and hash the worker with its clone and with a
///   second worker.
/// - Stop it; expect `true`, `Failed` status, rejected sends and `true` on a second stop.
/// - Stop the second worker with a zero timeout; expect `true` and 2 shutdowns.
#[ntex::test]
async fn worker_lifecycle() {
    let c = Counters::default();
    let (tx, rx) = mpsc::channel();
    let mut cfg = Cfg::new(&c);
    cfg.items = Some(tx);

    let mut wrk = Worker::start("wrk:0".to_string(), cfg.clone(), None);
    assert_eq!(wrk.name(), "wrk:0");
    assert_eq!(wrk.wait_for_status().await, WorkerStatus::Available);
    assert!(format!("{wrk:?}").contains("wrk:0"));

    assert!(wrk.send(1).is_ok());
    assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);

    let wrk2 = Worker::start("wrk:1".to_string(), cfg, None);
    assert_eq!(wrk, wrk.clone());
    assert_ne!(wrk, wrk2);
    assert!(wrk < wrk2);
    let hash = |w: &Worker<usize>| {
        let mut h = DefaultHasher::new();
        w.hash(&mut h);
        h.finish()
    };
    assert_eq!(hash(&wrk), hash(&wrk.clone()));
    assert_ne!(hash(&wrk), hash(&wrk2));

    let stop = wrk.stop(Millis(1000));
    assert!(stop.await);
    while wrk.wait_for_status().await != WorkerStatus::Failed {}
    assert_eq!(wrk.wait_for_status().await, WorkerStatus::Failed);
    assert_eq!(wrk.status(), WorkerStatus::Failed);
    wait_for(|| wrk.send(2).is_err()).await;
    // the worker is already gone
    assert!(wrk.stop(Millis(1000)).await);

    // zero timeout uses the default timeout
    assert!(wrk2.stop(Millis::ZERO).await);
    assert_eq!(c.shutdown.load(Relaxed), 2);
}

/// STATEMENT: Stopping a worker before its service is created reports an incomplete stop.
///
/// HOW:
/// - Start a worker whose service takes 5 s to create; expect `Unavailable`.
/// - Stop it with a 100 ms timeout; expect `false` and no created service.
#[ntex::test]
async fn worker_stop_before_start() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.create_delay = Millis(5000);

    let wrk: Worker<usize> = Worker::start("wrk:slow".to_string(), cfg, None);
    assert_eq!(wrk.status(), WorkerStatus::Unavailable);
    sleep(Millis(50)).await;
    assert!(!wrk.stop(Millis(100)).await);
    assert_eq!(c.created.load(Relaxed), 0);
}

/// STATEMENT: A worker whose shutdown exceeds the stop timeout reports an incomplete stop.
///
/// HOW:
/// - Start a worker whose shutdown takes 5 s and wait until it is `Available`.
/// - Stop it with a 50 ms timeout and expect `false`.
#[ntex::test]
async fn worker_stop_timeout() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.shutdown_delay = Millis(5000);

    let mut wrk: Worker<usize> = Worker::start("wrk:stop".to_string(), cfg, None);
    assert_eq!(wrk.wait_for_status().await, WorkerStatus::Available);
    assert!(!wrk.stop(Millis(50)).await);
}

/// STATEMENT: A service creation failure is reported as the `Failed` worker status.
///
/// HOW:
/// - Start a worker whose service creation fails.
/// - Expect `wait_for_status` to report `Failed` within 10 status changes.
/// - Check the default `WorkerStatus` is `Unavailable`.
#[ntex::test]
async fn worker_create_failure() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.fail_create = 1;

    let mut wrk: Worker<usize> = Worker::start("wrk:fail".to_string(), cfg, None);
    let mut failed = false;
    for _ in 0..10 {
        if wrk.wait_for_status().await == WorkerStatus::Failed {
            failed = true;
            break;
        }
    }
    assert!(failed);
    assert_eq!(WorkerStatus::default(), WorkerStatus::Unavailable);
}

/// STATEMENT: Awaiting a server resolves while its stop is in progress and immediately
/// after it has stopped.
///
/// HOW:
/// - Start a pool, process an item and begin a graceful stop.
/// - Await a server clone within 3 s, finish the stop, then await another clone within 3 s.
#[ntex::test]
async fn pool_await_after_stop() {
    let c = Counters::default();
    let srv = pool().workers(1).run(Cfg::new(&c));
    process(&srv, 1).await;

    // stop is in progress while the server is awaited
    let stop = srv.stop(true);
    ntex::time::timeout(Millis(3000), srv.clone())
        .await
        .expect("server is not stopped")
        .unwrap();
    stop.await;
    ntex::time::timeout(Millis(3000), srv.clone())
        .await
        .expect("server is not stopped")
        .unwrap();
}

/// STATEMENT: Awaiting a server stopped by a worker failure resolves immediately.
///
/// HOW:
/// - Start a `stop_on_panic` pool whose service creation fails and wait for the stop hook.
/// - Await a server clone and a later `stop` within 3 s each; expect one stop.
#[ntex::test]
async fn pool_await_after_stop_on_failure() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.fail_create = 1;
    let srv = pool().workers(1).stop_on_panic().run(cfg);

    wait_for(|| c.stop.load(Relaxed) == 1).await;
    sleep(Millis(100)).await;
    ntex::time::timeout(Millis(3000), srv.clone())
        .await
        .expect("server is not stopped")
        .unwrap();
    ntex::time::timeout(Millis(3000), srv.stop(true))
        .await
        .expect("stop is not completed");
    assert_eq!(c.stop.load(Relaxed), 1);
}

/// STATEMENT: A worker pool rejects a zero worker count instead of starting a server that
/// never becomes ready.
///
/// HOW:
/// - Call `WorkerPool::workers(0)`.
/// - Expect a panic naming the one-worker requirement.
#[test]
#[should_panic(expected = "at least one worker")]
fn zero_workers_are_rejected() {
    let _ = WorkerPool::new().workers(0);
}
