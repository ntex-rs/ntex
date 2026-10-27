use std::sync::atomic::{AtomicUsize, Ordering};

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
