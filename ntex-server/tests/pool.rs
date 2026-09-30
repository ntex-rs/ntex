//! Worker pool and worker integration tests.
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};
use std::{io, time::Duration};

use ntex::time::{Millis, sleep};
use ntex_server::{Server, ServerConfiguration, Worker, WorkerPool, WorkerStatus};
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
}

impl Counters {
    fn items(&self) -> Vec<usize> {
        self.items.lock().unwrap().clone()
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
    /// Panic when processing this item.
    panic_on: Option<usize>,
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
            panic_on: None,
            items: None,
        }
    }
}

struct Srv {
    cfg: Cfg,
}

impl Service<(), usize> for Srv {
    type Res = ();
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        if self.cfg.c.ready_failures.load(Relaxed) < self.cfg.fail_ready {
            self.cfg.c.ready_failures.fetch_add(1, Relaxed);
            Err(())
        } else {
            Ok(())
        }
    }

    async fn call(&self, item: usize, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        assert!(self.cfg.panic_on != Some(item), "item {item}");
        self.cfg.c.items.lock().unwrap().push(item);
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

async fn wait_for(f: impl Fn() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        sleep(Millis(10)).await;
    }
    panic!("condition is not met");
}

#[ntex::test]
async fn pool_process_items() {
    let c = Counters::default();
    let srv = pool()
        .workers(2)
        .graceful_shutdown_timeout(Millis(500))
        .run(Cfg::new(&c));
    assert!(format!("{srv:?}").contains("Server"));

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

/// A zero timeout makes a graceful stop non-graceful.
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

/// Workers that exceed the graceful timeout are abandoned.
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

/// A worker that fails to start is restarted.
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
    srv.stop(false).await;
}

/// A failed worker stops the server with `stop_on_panic`.
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

/// A worker that panics is restarted.
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
    srv.stop(false).await;
}

/// A failed readiness check re-creates the service.
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

/// `stop_runtime` stops the current system with the server.
#[test]
fn pool_stop_runtime() {
    let c = Counters::default();
    let c2 = c.clone();
    let sys = ntex::rt::System::new("stop-runtime", ntex::rt::DefaultRuntime);
    let res = sys.run(move || {
        let srv = pool()
            .workers(1)
            .enable_affinity()
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

#[test]
fn pool_builder() {
    let pool = WorkerPool::default()
        .name("test")
        .workers(3)
        .enable_affinity()
        .graceful_shutdown();
    let s = format!("{pool:?}");
    assert!(s.contains("\"test\"") && s.contains("num: 3"), "{s}");
}

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
    assert!(format!("{stop:?}").contains("WorkerStop"));
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

/// A worker stopped before its service is created reports an incomplete stop.
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

/// Shutdown that exceeds the timeout reports an incomplete stop.
#[ntex::test]
async fn worker_stop_timeout() {
    let c = Counters::default();
    let mut cfg = Cfg::new(&c);
    cfg.shutdown_delay = Millis(5000);

    let mut wrk: Worker<usize> = Worker::start("wrk:stop".to_string(), cfg, None);
    assert_eq!(wrk.wait_for_status().await, WorkerStatus::Available);
    assert!(!wrk.stop(Millis(50)).await);
}

/// A service creation failure is reported as the failed status.
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

/// Awaiting a server after it has stopped resolves immediately.
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

/// Awaiting a server stopped by a worker failure resolves immediately.
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
