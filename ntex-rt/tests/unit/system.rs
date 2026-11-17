use std::sync::atomic::{AtomicUsize, Ordering};

use futures_timer::Delay;

use super::*;
use crate::testing::TestRunner;

/// STATEMENT: Stopped arbiters are removed from the system's ping records.
///
/// HOW:
/// - Wait for a worker to answer a ping, then stop and join it.
/// - Confirm its recorded ping history is eventually removed.
#[test]
fn ping_records_pruned() {
    System::build()
        .name("test")
        .ping_interval(5)
        .build(TestRunner)
        .block_on(async {
            let mut arb = Arbiter::new();
            let id = arb.id();
            let answered = || {
                PINGS.with(|p| {
                    p.borrow()
                        .get(&id)
                        .is_some_and(|recs| recs.iter().any(|r| r.rtt.is_some()))
                })
            };
            let has = || PINGS.with(|p| p.borrow().contains_key(&id));

            for _ in 0..400 {
                if answered() {
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            assert!(answered());

            arb.stop();
            arb.join().unwrap();
            for _ in 0..400 {
                if !has() {
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            assert!(!has());
        });
}

/// STATEMENT: Latency sampling covers primary and worker arbiters and can be disabled.
///
/// HOW:
/// - Wait for multiple samples on both arbiters and inspect the interval and timestamp.
/// - Build with sampling disabled and verify neither arbiter exposes latency metrics.
#[test]
fn latency_sampling() {
    System::build()
        .name("test")
        .latency_interval(5)
        .build(TestRunner)
        .block_on(async {
            let mut arb = Arbiter::new();
            let sampled = |arb: &Arbiter| arb.latency().unwrap().histogram.count() >= 2;
            let sys_arb = System::current().arbiter();

            for _ in 0..400 {
                if sampled(&arb) && sampled(&sys_arb) {
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            assert!(sampled(&arb));
            assert!(sampled(&sys_arb));

            let m = arb.latency().unwrap();
            assert_eq!(m.interval, Duration::from_millis(5));
            assert!(m.last_sample.unwrap() <= Instant::now());

            arb.stop();
            arb.join().unwrap();
        });

    System::build()
        .name("test")
        .latency_interval(0)
        .build(TestRunner)
        .block_on(async {
            assert_eq!(System::current().arbiter().latency(), None);
            let mut arb = Arbiter::new();
            assert_eq!(arb.latency(), None);
            arb.stop();
            arb.join().unwrap();
        });
}

/// STATEMENT: Required current-system access panics when no system is installed.
///
/// HOW:
/// - Verify optional access returns None, then check the required accessor's panic.
#[test]
#[should_panic(expected = "System is not running")]
fn current_without_system() {
    assert!(System::try_current().is_none());
    let _ = System::current();
}

/// STATEMENT: System identity, stored values, execution, and reusable configuration stay consistent.
///
/// HOW:
/// - Inspect the current system, initialize a value twice, and run local and blocking work.
/// - Finish the system and reuse its configuration under a new name.
#[test]
fn system_api() {
    let config = System::new("base", TestRunner).block_on(async {
        let sys = System::current();
        assert_eq!(sys.name(), "base");
        assert!(!sys.testing());
        assert!(!sys.signals());
        assert_eq!(System::try_current().unwrap().id(), sys.id());
        assert_eq!(sys.arbiter(), Arbiter::current());
        assert!(format!("{sys:?}").contains("base"));
        assert!(format!("{:?}", sys.config()).contains("base"));

        // values are stored once per system
        assert_eq!(sys.get_value(|| 1usize), 1);
        assert_eq!(sys.get_value(|| 2usize), 1);

        assert_eq!(sys.spawn_blocking(|| 10).await, Ok(10));

        let cnt = Arc::new(AtomicUsize::new(0));
        let cnt2 = cnt.clone();
        sys.handle()
            .spawn(async move {
                cnt2.fetch_add(1, Ordering::Relaxed);
            })
            .await
            .unwrap();
        assert_eq!(cnt.load(Ordering::Relaxed), 1);
        sys.list_arbiters(|arbs| assert_eq!(arbs, [sys.arbiter()]));
        sys.config()
    });
    assert!(System::try_current().is_none());

    System::with_config("copy", config).block_on(async {
        let sys = System::current();
        assert_eq!(sys.name(), "copy");
        assert_eq!(Arbiter::current().name(), "copy");
    });
}

/// STATEMENT: Ping enumeration reports answered pings without holding the arbiter registry lock.
///
/// HOW:
/// - Wait until a worker's answered ping is visible through enumeration.
/// - Acquire the registry lock inside an enumeration callback.
#[test]
fn list_arbiter_pings() {
    System::build()
        .name("test")
        .ping_interval(5)
        .build(TestRunner)
        .block_on(async {
            let mut arb = Arbiter::new();
            let visited = || {
                let mut found = false;
                System::list_arbiter_pings(|a, recs| {
                    if a == &arb && recs.iter().any(|r| r.rtt.is_some()) {
                        found = true;
                    }
                });
                found
            };
            for _ in 0..400 {
                if visited() {
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            assert!(visited());

            // callbacks may access the arbiter registry
            let mut visited = false;
            System::list_arbiter_pings(|_, _| {
                visited = true;
                assert!(System::current().0.arbiters.try_lock().is_some());
            });
            assert!(visited);

            arb.stop();
            arb.join().unwrap();
        });
}

/// STATEMENT: Arbiter enumeration permits callbacks to stop, join, and enumerate workers.
///
/// HOW:
/// - Stop and join a listed worker inside the callback.
/// - Verify the registry is unlocked and a nested enumeration excludes the stopped worker.
#[test]
fn list_arbiters_callback_joins_worker() {
    System::new("list-join", TestRunner).block_on(async {
        let sys = System::current();
        let worker = Arbiter::new();
        sys.list_arbiters(|arbs| {
            // the registry is not locked while the callback runs
            assert!(sys.0.arbiters.try_lock().is_some());
            let mut arb = arbs.iter().find(|arb| **arb == worker).unwrap().clone();
            arb.stop();
            arb.join().unwrap();
            sys.list_arbiters(|arbs| assert!(!arbs.contains(&worker)));
        });
    });
}

/// STATEMENT: Blocking work runs on the pool of the submitting arbiter, and
/// `System::pool_metrics` sums the arbiter pools, keeping totals of stopped ones.
///
/// HOW:
/// - Submit work from the primary arbiter, a spawned arbiter, a plain thread,
///   and for another system, and check the name prefix of the pool thread
///   that ran it.
/// - Compare per-arbiter and system task counts and thread limits, then stop
///   the spawned arbiter and check its tasks remain in the system totals.
#[test]
fn spawn_blocking_uses_arbiter_pool() {
    fn pool_thread() -> String {
        std::thread::current().name().unwrap().to_string()
    }

    let (other_tx, other_rx) = std::sync::mpsc::channel();
    let other_thread = std::thread::spawn(move || {
        System::new("other", TestRunner).block_on(async move {
            let (stop_tx, stop_rx) = oneshot::async_channel::<()>();
            other_tx.send((System::current(), stop_tx)).unwrap();
            let _ = stop_rx.await;
        });
    });
    let (other, other_stop) = other_rx.recv().unwrap();

    System::build()
        .name("pools")
        .thread_pool_limit(3)
        .build(TestRunner)
        .block_on(async move {
            let sys = System::current();
            assert!(
                crate::spawn_blocking(pool_thread)
                    .await
                    .unwrap()
                    .starts_with("pools:pool-wrk:")
            );
            // another system's work runs on that system's pool
            assert!(
                other
                    .spawn_blocking(pool_thread)
                    .await
                    .unwrap()
                    .starts_with("other:pool-wrk:")
            );
            drop(other_stop);

            let mut arb = Arbiter::with_name("pools-arb".to_string());
            let names = arb
                .handle()
                .spawn(async {
                    let local = crate::spawn_blocking(pool_thread).await.unwrap();
                    let sys = System::current().spawn_blocking(pool_thread).await.unwrap();
                    (local, sys)
                })
                .await
                .unwrap();
            assert!(names.0.starts_with("pools-arb:pool-wrk:"), "{names:?}");
            assert!(names.1.starts_with("pools-arb:pool-wrk:"), "{names:?}");

            // a thread without an arbiter uses the primary arbiter's pool
            let sys2 = sys.clone();
            let name = std::thread::spawn(move || sys2.spawn_blocking(pool_thread))
                .join()
                .unwrap()
                .await
                .unwrap();
            assert!(name.starts_with("pools:pool-wrk:"), "{name}");

            assert_eq!(arb.pool_metrics().tasks, 2);
            assert_eq!(sys.arbiter().pool_metrics().tasks, 2);
            let m = sys.pool_metrics();
            assert_eq!((m.thread_limit, m.tasks), (6, 4));

            arb.stop();
            arb.join().unwrap();
            let m = sys.pool_metrics();
            assert_eq!((m.thread_limit, m.tasks), (3, 4));
        });
    other_thread.join().unwrap();
}

/// STATEMENT: Blocking pool threads do not inherit CPU affinity set by an
/// arbiter's `init`.
///
/// HOW:
/// - Skip unless the test thread may run on at least two CPUs.
/// - Start an arbiter whose `init` pins its thread to one CPU, then submit
///   blocking work from it, so the pinned thread starts the pool thread.
/// - Confirm the arbiter thread runs on that CPU only and the pool thread on
///   all CPUs of the test thread.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn blocking_pool_ignores_arbiter_affinity() {
    fn cpus() -> Vec<usize> {
        // SAFETY: cpu_set_t is plain data, the size matches the set
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            assert_eq!(
                libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &raw mut set),
                0
            );
            // `CPU_SETSIZE` is a `c_int` on Linux and a `usize` on Android
            let size = u32::try_from(libc::CPU_SETSIZE).unwrap() as usize;
            (0..size).filter(|&n| libc::CPU_ISSET(n, &set)).collect()
        }
    }

    let all = cpus();
    if all.len() < 2 {
        return;
    }
    let pinned = all[0];

    System::build()
        .name("affinity")
        .build(TestRunner)
        .block_on(async move {
            let mut arb = Arbiter::with_init("affinity-arb".to_string(), move || {
                // SAFETY: cpu_set_t is plain data, the size matches the set
                unsafe {
                    let mut set: libc::cpu_set_t = std::mem::zeroed();
                    libc::CPU_SET(pinned, &mut set);
                    assert_eq!(
                        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set),
                        0
                    );
                }
            });
            let (arb_cpus, pool_cpus) = arb
                .handle()
                .spawn(async { (cpus(), crate::spawn_blocking(cpus).await.unwrap()) })
                .await
                .unwrap();
            assert_eq!(arb_cpus, [pinned]);
            assert_eq!(pool_cpus, all);

            arb.stop();
            arb.join().unwrap();
        });
}
