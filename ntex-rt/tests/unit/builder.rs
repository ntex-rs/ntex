use std::sync::atomic::{AtomicUsize, Ordering};
use std::{cell::Cell, cell::RefCell, time::Duration};

use super::*;
use crate::{Arbiter, testing::TestRunner};

/// STATEMENT: Builder options configure the system and its arbiter and blocking pool.
///
/// HOW:
/// - Build with explicit identity, flags, stack size, and pool settings.
/// - Inspect the running system, start an arbiter, and execute blocking work.
#[test]
fn builder_options() {
    let builder = System::build()
        .name("opts")
        .signals(false)
        .panic_handling(false)
        .enable_signals()
        .disable_signals()
        .stack_size(2 * 1024 * 1024)
        .ping_interval(0)
        .ping_threshold(10)
        .thread_pool_limit(1)
        .thread_pool_recv_timeout(Duration::from_millis(100))
        .testing();
    assert!(format!("{builder:?}").contains("opts"));

    let runner = builder.build(TestRunner);
    assert!(format!("{runner:?}").contains("opts"));
    runner.block_on(async {
        let sys = System::current();
        assert_eq!(sys.name(), "opts");
        assert!(sys.testing());
        assert!(!sys.signals());

        // arbiter thread with configured stack size
        let mut arb = Arbiter::new();
        arb.stop();
        arb.join().unwrap();

        assert_eq!(sys.spawn_blocking(|| 1).await, Ok(1));
    });
}

/// STATEMENT: Running a system propagates initialization and exit errors while cleaning up.
///
/// HOW:
/// - Exercise initialization failure, normal stop, and a nonzero exit code.
/// - Confirm shutdown callbacks ran and child arbiters stopped.
#[test]
fn run_exit_codes() {
    // `f` error is returned
    let err = System::new("test", TestRunner)
        .run(|| Err(io::Error::other("init failed")))
        .unwrap_err();
    assert_eq!(err.to_string(), "init failed");

    System::new("test", TestRunner)
        .run(|| {
            System::current().stop();
            Ok(())
        })
        .unwrap();

    // stopping the system stops arbiters and runs shutdown callbacks
    let called = Rc::new(Cell::new(false));
    let arb = Rc::new(RefCell::new(None));
    let (called2, arb2) = (called.clone(), arb.clone());
    let err = System::new("test", TestRunner)
        .run(move || {
            Arbiter::on_shutdown(move || called2.set(true));
            *arb2.borrow_mut() = Some(Arbiter::new());
            System::current().stop_with_code(3);
            Ok(())
        })
        .unwrap_err();
    assert_eq!(err.to_string(), "Non-zero exit code: 3");
    assert!(called.get());

    let mut arb = arb.borrow_mut().take().unwrap();
    arb.join().unwrap();
    assert!(!arb.is_running());
}

/// STATEMENT: All root exit paths stop child arbiters.
///
/// HOW:
/// - Exit through initialization failure, root completion, and root panic.
/// - Join each child with a bounded wait and confirm shutdown needs no explicit child stop.
#[test]
fn exit_stops_child_arbiters() {
    fn assert_stopped(arb: &Rc<RefCell<Option<Arbiter>>>) {
        let arb = arb.borrow_mut().take().unwrap();
        let mut arb2 = arb.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(arb2.join().is_ok());
        });
        let stopped = rx.recv_timeout(Duration::from_secs(5));
        arb.stop();
        assert_eq!(stopped, Ok(true), "child arbiter is still running");
    }

    // initialization error
    let arb = Rc::new(RefCell::new(None));
    let arb2 = arb.clone();
    let err = System::new("test", TestRunner)
        .run(move || {
            *arb2.borrow_mut() = Some(Arbiter::new());
            Err(io::Error::other("init failed"))
        })
        .unwrap_err();
    assert_eq!(err.to_string(), "init failed");
    assert_stopped(&arb);

    // root future completion
    let arb2 = arb.clone();
    System::new("test", TestRunner).block_on(async move {
        *arb2.borrow_mut() = Some(Arbiter::new());
    });
    assert_stopped(&arb);

    // root future panic
    let arb2 = arb.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        System::new("test", TestRunner).block_on(async move {
            *arb2.borrow_mut() = Some(Arbiter::new());
            panic!("root future panic");
        });
    }));
    assert!(result.is_err());
    assert_stopped(&arb);
}

/// STATEMENT: The primary arbiter is finalized exactly once when the system exits.
///
/// HOW:
/// - Retain the primary arbiter through root completion, root panic, and `run` shutdown.
/// - Verify it is stopped and its on-stop callback ran once in every case.
#[test]
fn primary_arbiter_finalized() {
    fn with_on_stop(calls: &Arc<AtomicUsize>) -> Arbiter {
        let calls = calls.clone();
        Arbiter::current().on_stop(move || {
            calls.fetch_add(1, Ordering::Relaxed);
        })
    }

    // root future completion
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let arb = System::new("test", TestRunner).block_on(async move { with_on_stop(&calls2) });
    assert!(!arb.is_running());
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // root future panic
    let calls = Arc::new(AtomicUsize::new(0));
    let (calls2, slot) = (calls.clone(), Rc::new(RefCell::new(None)));
    let slot2 = slot.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        System::new("test", TestRunner).block_on(async move {
            *slot2.borrow_mut() = Some(with_on_stop(&calls2));
            panic!("root future panic");
        });
    }));
    assert!(result.is_err());
    assert!(!slot.borrow().as_ref().unwrap().is_running());
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    // `run`
    let calls = Arc::new(AtomicUsize::new(0));
    let (calls2, slot) = (calls.clone(), Rc::new(RefCell::new(None)));
    let slot2 = slot.clone();
    System::new("test", TestRunner)
        .run(move || {
            *slot2.borrow_mut() = Some(with_on_stop(&calls2));
            System::current().stop();
            Ok(())
        })
        .unwrap();
    assert!(!slot.borrow().as_ref().unwrap().is_running());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[cfg(feature = "tokio")]
/// STATEMENT: Tokio's local runner finalizes the primary arbiter on completion.
///
/// HOW:
/// - Run a system locally in a current-thread Tokio runtime and retain its arbiter.
/// - Confirm the arbiter is stopped and its on-stop callback ran once.
#[test]
fn run_local_finalizes_primary_arbiter() {
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let rt = tok_io::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let arb = rt.block_on(System::new("test", TestRunner).run_local(async move {
        Arbiter::current().on_stop(move || {
            calls2.fetch_add(1, Ordering::Relaxed);
        })
    }));
    assert!(!arb.is_running());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}
