use std::sync::atomic::{AtomicUsize, Ordering};
use std::{panic::Location, sync::Arc};

use futures_timer::Delay;

use super::*;
use crate::testing::TestRunner;

/// Serializes tests that stall arbiters or set the global latency callback.
static CALLBACK: Mutex<()> = Mutex::new(());

/// STATEMENT: The watchdog attributes stalls to the blocked arbiter and records recovery.
///
/// HOW:
/// - Block a worker task long enough to trigger detection and check its spawn location when available.
/// - Await recovery and verify one recorded stall without attributing it to the primary arbiter.
#[test]
fn stall_detection() {
    let _guard = CALLBACK.lock();
    // parked threads on CI VMs (macOS) can wake up tens of ms late
    System::build()
        .name("test")
        .latency_interval(10)
        .ping_threshold(200)
        .build(TestRunner)
        .block_on(async {
            let mut arb = Arbiter::new();
            let line = line!() + 1;
            let done = arb.handle().spawn(async move {
                std::thread::sleep(Duration::from_millis(800));
            });

            let stalled = || arb.latency().unwrap().stalled;
            for _ in 0..400 {
                if stalled() {
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            assert!(stalled());
            // the watchdog names the blocking task
            let task = arb.handle().current_task();
            if cfg!(any(feature = "tokio", feature = "compio")) {
                assert_eq!(task, None);
            } else {
                let task = task.unwrap();
                assert_eq!((task.file(), task.line()), (file!(), line));
            }
            done.await.unwrap();

            for _ in 0..400 {
                if !stalled() {
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            let m = arb.latency().unwrap();
            assert!(!m.stalled);
            assert_eq!(m.stalls, 1);
            assert!(m.stall_duration >= Duration::from_millis(500), "{m:?}");

            // the stall is attributed to the blocked arbiter only
            let sys = System::current().arbiter().latency().unwrap();
            assert!(!sys.stalled);
            assert!(sys.stall_duration < m.stall_duration, "{sys:?}");

            arb.stop();
            arb.join().unwrap();
        });
}

/// STATEMENT: A running latency callback can replace itself without invalidating its captures.
///
/// HOW:
/// - Replace the callback from inside itself, then access its captured allocation.
/// - Invoke the replacement and verify the original callback ran only once.
#[test]
fn latency_callback_replaced_while_running() {
    let _guard = CALLBACK.lock();
    let calls = Arc::new(AtomicUsize::new(0));
    let data = Arc::new(vec![1u8; 64]);
    let calls2 = calls.clone();
    System::set_latency_callback(move |_, _| {
        // replace itself, then use captured state
        System::set_latency_callback(|_, _| {});
        assert_eq!(data.len(), 64);
        calls2.fetch_add(1, Ordering::Relaxed);
    });

    latency_callback().unwrap()(ntex_error::Backtrace::new(Location::caller()), None);
    latency_callback().unwrap()(ntex_error::Backtrace::new(Location::caller()), None);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    *ARB_CB.lock() = None;
}

#[cfg(all(windows, any(target_arch = "x86_64", target_arch = "aarch64")))]
// Spins instead of blocking, a blocking call can be a tail call that
// leaves no frame of this function. Exported, so the symbol is resolved
// without debug info.
#[inline(never)]
#[unsafe(no_mangle)]
extern "C" fn ntex_rt_stalled_arbiter_fn(stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
}

#[cfg(all(windows, any(target_arch = "x86_64", target_arch = "aarch64")))]
/// STATEMENT: Windows stall reports capture the blocked frame and its task location when available.
///
/// HOW:
/// - Stall an arbiter in an exported function and collect watchdog backtraces.
/// - Check the stalled function, caller, and task location, then release the task and await recovery.
#[test]
fn stall_backtrace() {
    let _guard = CALLBACK.lock();
    let (bt_tx, bt_rx) = std::sync::mpsc::channel();
    let bt_tx = Mutex::new(bt_tx);
    System::set_latency_callback(move |bt, task| {
        let _ = bt_tx.lock().send((format!("{:?}", bt.resolve()), task));
    });

    System::build()
        .name("test")
        .latency_interval(10)
        .ping_threshold(50)
        .build(TestRunner)
        .block_on(async move {
            let mut arb = Arbiter::new();
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            let line = line!() + 1;
            let done = arb.handle().spawn(async move {
                ntex_rt_stalled_arbiter_fn(&stop2);
            });

            // other tests can stall their arbiters too
            let mut found = None;
            for _ in 0..2000 {
                if let Ok((bt, task)) = bt_rx.try_recv()
                    && bt.contains("ntex_rt_stalled_arbiter_fn")
                {
                    found = Some((bt, task));
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            stop.store(true, Ordering::Release);
            done.await.unwrap();
            let (bt, task) = found.expect("backtrace of the stalled arbiter");
            assert!(bt.contains("stall_backtrace"), "{bt}");
            if cfg!(any(feature = "tokio", feature = "compio")) {
                assert_eq!(task, None);
            } else {
                let task = task.unwrap();
                assert_eq!((task.file(), task.line()), (file!(), line));
            }

            let recovered = || !arb.latency().unwrap().stalled;
            for _ in 0..400 {
                if recovered() {
                    break;
                }
                Delay::new(Duration::from_millis(5)).await;
            }
            assert!(recovered());
            arb.stop();
            arb.join().unwrap();
        });
    *ARB_CB.lock() = None;
}
