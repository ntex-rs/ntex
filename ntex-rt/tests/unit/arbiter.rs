use std::cell::Cell;

use super::*;
use crate::testing::TestRunner;

#[derive(Clone, Default)]
struct Value(usize);

struct DropTx(Option<oneshot::Sender<thread::ThreadId>>);

impl Drop for DropTx {
    fn drop(&mut self) {
        let _ = self.0.take().unwrap().send(thread::current().id());
    }
}

/// STATEMENT: Accessing the current arbiter without an arbiter panics.
///
/// HOW:
/// - Call `Arbiter::current` outside a running system and check the panic.
#[test]
#[should_panic(expected = "Arbiter is not running")]
fn current_without_arbiter() {
    let _ = Arbiter::current();
}

/// STATEMENT: The initialization callback runs on the new arbiter's thread.
///
/// HOW:
/// - Set thread-local state and report the thread name from the callback.
/// - Read the state from a spawned task and confirm the caller's state is unchanged.
#[test]
fn arbiter_with_init() {
    thread_local! {
        static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    System::new("arb-init", TestRunner).block_on(async {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut arb = Arbiter::with_init("init".to_string(), move || {
            INIT.with(|init| init.set(true));
            let _ = tx.send(thread::current().name().map(str::to_string));
        });
        assert_eq!(rx.recv().unwrap().as_deref(), Some("init"));

        // `init` ran on the arbiter's thread
        let (tx, rx) = oneshot::async_channel();
        arb.handle().spawn(async move {
            let _ = tx.send(INIT.with(std::cell::Cell::get));
        });
        assert!(rx.await.unwrap());
        assert!(!INIT.with(std::cell::Cell::get));

        arb.stop();
        arb.join().unwrap();
    });
}

/// STATEMENT: Arbiters expose their identity and clean up registration and callbacks on stop.
///
/// HOW:
/// - Inspect the primary, named, and default arbiters and their registry entries.
/// - Stop and join a worker, checking its callbacks and removal from the registry.
#[test]
fn arbiter_api() {
    System::new("arb-test", TestRunner).block_on(async {
        let sys = System::current();
        let cur = Arbiter::current();
        assert_eq!(cur.name(), "arb-test");
        assert_eq!(cur, sys.arbiter());
        assert!(cur.is_running());
        assert_eq!(format!("{cur:?}"), "Arbiter(\"arb-test\")");
        // the primary arbiter does not own a thread
        assert!(cur.clone().join().is_ok());

        let stopped = Arc::new(AtomicUsize::new(0));
        let stopped2 = stopped.clone();
        let mut arb = Arbiter::with_name("named".to_string()).on_stop(move || {
            stopped2.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(arb.name(), "named");
        assert!(arb.is_running());
        assert_ne!(arb, cur);
        sys.list_arbiters(|arbs| assert!(arbs.contains(&arb)));

        // shutdown callbacks run on the arbiter's thread
        let (reg_tx, reg_rx) = oneshot::async_channel();
        let (tx, rx) = oneshot::channel();
        arb.handle().spawn(async move {
            let name = Arbiter::current().name().to_string();
            Arbiter::on_shutdown(move || {
                let _ = tx.send(name);
            });
            let _ = reg_tx.send(());
        });
        reg_rx.await.unwrap();

        arb.stop();
        arb.join().unwrap();
        assert!(!arb.is_running());
        assert_eq!(stopped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.recv().unwrap(), "named");
        sys.list_arbiters(|arbs| assert!(!arbs.contains(&arb)));

        let mut arb = Arbiter::default();
        assert!(arb.name().starts_with("arb-test:arb:"));
        arb.stop();
        arb.join().unwrap();
    });
}

/// STATEMENT: Local handles retain completed results and report cancellation after shutdown.
///
/// HOW:
/// - Keep completed and pending handles beyond the runner's lifetime.
/// - Consume or cancel the result and verify each value is dropped once on its owner.
#[test]
fn local_handles_survive_runtime_shutdown() {
    struct Local {
        owner: thread::ThreadId,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Local {
        fn drop(&mut self) {
            assert_eq!(thread::current().id(), self.owner);
            self.drops.set(self.drops.get() + 1);
        }
    }

    for consume in [false, true] {
        let drops = Rc::new(Cell::new(0));
        let value = Local {
            owner: thread::current().id(),
            drops: drops.clone(),
        };
        let guard = Local {
            owner: thread::current().id(),
            drops: drops.clone(),
        };
        let handles = Rc::new(Cell::new(None));
        let slot = handles.clone();
        crate::Runner::block_on(
            &TestRunner,
            Box::pin(async move {
                let completed = crate::spawn(async move { value });
                let cancelled = crate::spawn(async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                });
                std::future::poll_fn(|cx| {
                    if completed.is_finished() {
                        std::task::Poll::Ready(())
                    } else {
                        cx.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                })
                .await;
                slot.set(Some((completed, cancelled)));
            }),
        )
        .unwrap();
        let (completed, mut cancelled) = handles.take().unwrap();
        assert!(completed.is_finished());
        assert!(cancelled.is_finished());
        assert_eq!(drops.get(), 1);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            std::pin::Pin::new(&mut cancelled).poll(&mut cx),
            std::task::Poll::Ready(Err(_))
        ));
        drop(cancelled);
        if consume {
            let mut completed = completed;
            let std::task::Poll::Ready(Ok(value)) =
                std::pin::Pin::new(&mut completed).poll(&mut cx)
            else {
                panic!("completed local result lost during shutdown");
            };
            drop(completed);
            assert_eq!(drops.get(), 1);
            drop(value);
        } else {
            completed.cancel();
        }
        assert_eq!(drops.get(), 2);
    }
}

/// STATEMENT: A detached remote task can produce and drop a non-Send output on its owner.
///
/// HOW:
/// - Submit a future that creates an Rc-backed output on another arbiter.
/// - Check the output's destructor runs there and detached submission returns unit.
#[test]
fn detached_remote_output_is_owner_local() {
    struct Output {
        owner: thread::ThreadId,
        done: Option<oneshot::Sender<()>>,
        _local: Rc<()>,
    }
    impl Drop for Output {
        fn drop(&mut self) {
            assert_eq!(thread::current().id(), self.owner);
            let _ = self.done.take().unwrap().send(());
        }
    }
    // The closure's return type verifies that detached spawning returns ().
    fn submit(f: impl FnOnce()) {
        f();
    }

    System::new("remote-output", TestRunner).block_on(async {
        let mut arb = Arbiter::new();
        let (tx, rx) = oneshot::async_channel();
        let caller = thread::current().id();
        let future = async move {
            let owner = thread::current().id();
            assert_ne!(owner, caller);
            Output {
                owner,
                done: Some(tx),
                _local: Rc::new(()),
            }
        };
        submit(|| arb.handle().spawn_detached(future));
        rx.await.unwrap();
        arb.stop();
        arb.join().unwrap();
    });
}

/// STATEMENT: Remote joins preserve owner-thread execution, submission order, and detachment.
///
/// HOW:
/// - Check remote results, ordered detached submissions, and owner-thread cancellation.
/// - Exercise the local fast path and confirm dropped or detached handles do not cancel work.
#[test]
fn remote_join_handle() {
    System::new("remote-results", TestRunner).block_on(async {
        let mut arb = Arbiter::new();
        let task: crate::RemoteJoinHandle<_> = arb.handle().spawn(async { thread::current().id() });
        let id = task.await.unwrap();
        assert_ne!(id, thread::current().id());

        // spawns from one producer run in order on the arbiter thread
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..100 {
            let order = order.clone();
            arb.handle().spawn_detached(async move {
                order.lock().push((i, thread::current().id()));
            });
        }
        arb.handle().spawn(async {}).await.unwrap();
        let order = std::mem::take(&mut *order.lock());
        assert_eq!(order.len(), 100);
        assert!(
            order
                .iter()
                .enumerate()
                .all(|(i, (n, t))| i == *n && *t == id)
        );

        // a cancelled remote task is dropped on the arbiter thread
        let (tx, rx) = oneshot::async_channel();
        let guard = DropTx(Some(tx));
        let task = arb.handle().spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        assert!(!task.is_finished());
        task.cancel();
        assert_eq!(rx.await.unwrap(), id);

        // the owner spawns locally
        let local: crate::JoinHandle<_> = crate::spawn(async { Rc::new(42) });
        assert_eq!(*local.await.unwrap(), 42);
        let task: crate::RemoteJoinHandle<_> =
            Handle::current().spawn(async { thread::current().id() });
        assert_eq!(task.await.unwrap(), thread::current().id());

        let (tx, rx) = oneshot::async_channel();
        let guard = DropTx(Some(tx));
        Handle::current()
            .spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            })
            .cancel();
        assert_eq!(rx.await.unwrap(), thread::current().id());

        for detach in [false, true] {
            let (start, ready) = oneshot::async_channel();
            let (done, completed) = oneshot::async_channel();
            let task = Handle::current().spawn(async move {
                ready.await.unwrap();
                let _ = done.send(());
            });
            if detach {
                #[allow(deprecated, reason = "verify legacy detach matches handle drop")]
                task.detach();
            } else {
                drop(task);
            }
            start.send(()).unwrap();
            completed.await.unwrap();
        }

        let (start, ready) = oneshot::async_channel();
        let (done, completed) = oneshot::async_channel();
        let task = arb.handle().spawn(async move {
            ready.await.unwrap();
            let _ = done.send(());
        });
        drop(task);
        start.send(()).unwrap();
        completed.await.unwrap();
        arb.stop();
        arb.join().unwrap();
    });
}

/// STATEMENT: Arbiter shutdown cancels running remote tasks and rejects later submissions.
///
/// HOW:
/// - Stop the arbiter after a task becomes pending and observe its destructor and join error.
/// - Submit again and confirm the rejected future is not polled.
#[test]
fn handle_spawn_shutdown_while_running() {
    struct Token(Option<oneshot::Sender<()>>);
    impl Drop for Token {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    System::new("shutdown-running", TestRunner).block_on(async {
        let mut arb = Arbiter::new();
        let resumed = Arc::new(AtomicBool::new(false));
        let resumed2 = resumed.clone();
        let (started_tx, started_rx) = oneshot::async_channel();
        let (drop_tx, drop_rx) = oneshot::async_channel();
        let task = arb.handle().spawn(async move {
            let _token = Token(Some(drop_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
            resumed2.store(true, Ordering::Relaxed);
        });
        started_rx.await.unwrap();
        arb.stop();
        arb.join().unwrap();
        assert!(task.await.is_err());
        // the future is dropped on the arbiter thread
        let mut drop_rx = std::pin::pin!(drop_rx);
        let mut timeout =
            std::pin::pin!(futures_timer::Delay::new(std::time::Duration::from_secs(5)));
        std::future::poll_fn(|cx| {
            if drop_rx.as_mut().poll(cx).is_ready() {
                std::task::Poll::Ready(())
            } else if timeout.as_mut().poll(cx).is_ready() {
                panic!("shutdown task is not dropped");
            } else {
                std::task::Poll::Pending
            }
        })
        .await;
        assert!(!resumed.load(Ordering::Relaxed));

        // Rejected submissions close the result channel without running.
        let task = arb.handle().spawn(async { panic!("rejected task polled") });
        assert!(task.await.is_err());
    });
}

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
/// STATEMENT: A panicking arbiter reports its panic and still performs shutdown cleanup.
///
/// HOW:
/// - Panic in a task with runners that catch or propagate the event-loop panic.
/// - Check the join error, shutdown callback, stopped state, and registry removal.
#[test]
fn panicking_arbiter() {
    /// Returns a panic of the event loop as an error, like the ntex-net
    /// runners.
    struct CatchRunner;

    impl crate::Runner for CatchRunner {
        fn block_on(&self, fut: crate::BlockFuture) -> Result<(), Box<dyn std::any::Any + Send>> {
            panic::catch_unwind(panic::AssertUnwindSafe(|| TestRunner.block_on(fut)))?
        }
    }

    fn check(runner: impl crate::Runner) {
        System::new("arb-panic", runner).block_on(async {
            let sys = System::current();
            let mut arb = Arbiter::with_name("panics".to_string());
            let (tx, rx) = oneshot::channel();
            let task = arb.handle().spawn(async move {
                Arbiter::on_shutdown(move || {
                    let _ = tx.send(());
                });
                panic!("arbiter boom");
            });

            // the panic of the event loop is returned by `join`
            let err = arb.join().unwrap_err();
            assert_eq!(err.downcast_ref::<&str>(), Some(&"arbiter boom"));
            assert!(task.await.is_err());
            // the arbiter is unregistered and its shutdown callbacks ran
            assert!(rx.recv().is_ok());
            assert!(!arb.is_running());
            sys.list_arbiters(|arbs| assert!(!arbs.contains(&arb)));
        });
    }

    check(CatchRunner);
    // the panic unwinds through the runner
    check(TestRunner);
}

#[cfg(all(not(feature = "tokio"), not(feature = "compio")))]
/// STATEMENT: A second panic in arbiter cleanup aborts the process.
///
/// HOW:
/// - Run child processes that panic in a task and then in either shutdown callback.
/// - Check both panic messages and an abort exit rather than an ordinary test failure.
#[test]
#[cfg_attr(miri, ignore = "spawns a process")]
fn panicking_arbiter_cleanup_aborts() {
    const CHILD: &str = "NTEX_RT_ARBITER_CLEANUP_CHILD";

    struct CatchRunner;

    impl crate::Runner for CatchRunner {
        fn block_on(&self, fut: crate::BlockFuture) -> Result<(), Box<dyn std::any::Any + Send>> {
            panic::catch_unwind(panic::AssertUnwindSafe(|| TestRunner.block_on(fut)))?
        }
    }

    if let Ok(case) = std::env::var(CHILD) {
        System::new("arb-panic", CatchRunner).block_on(async move {
            let mut arb = Arbiter::with_name("panics".to_string());
            let on_stop = case == "on_stop";
            arb.handle().spawn(async move {
                if on_stop {
                    let _ = Arbiter::current().on_stop(|| panic!("cleanup panic"));
                } else {
                    Arbiter::on_shutdown(|| panic!("cleanup panic"));
                }
                panic!("arbiter boom");
            });
            let _ = arb.join();
        });
        return;
    }

    for case in ["on_stop", "on_shutdown"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "arbiter::tests::panicking_arbiter_cleanup_aborts",
                "--exact",
            ])
            .args(["--nocapture", "--test-threads=1"])
            .env(CHILD, case)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{case}: {stderr}");
        // a normal test failure exits with 101, an abort does not
        assert_ne!(out.status.code(), Some(101), "{case}: {stderr}");
        assert!(stderr.contains("arbiter boom"), "{case}: {stderr}");
        assert!(stderr.contains("cleanup panic"), "{case}: {stderr}");
    }
}

/// STATEMENT: An arbiter initializes each stored value type only once.
///
/// HOW:
/// - Request the same type with different factories and verify the first value is reused.
#[test]
fn arbiter_values() {
    System::new("test", TestRunner).block_on(async {
        assert_eq!(Arbiter::get_value(|| Value(1)).0, 1);
        assert_eq!(Arbiter::get_value(|| Value(2)).0, 1);
    });
}

/// STATEMENT: Shutdown runs callbacks registered by other shutdown callbacks.
///
/// HOW:
/// - Register a callback that registers another callback.
/// - Finish `block_on` and confirm both callbacks ran.
#[test]
fn on_shutdown_runs_after_block_on() {
    let called = Rc::new(Cell::new(0));
    let called2 = called.clone();
    System::new("test", TestRunner).block_on(async move {
        Arbiter::on_shutdown(move || {
            called2.set(called2.get() + 1);
            // registered by a callback, runs in the same shutdown
            Arbiter::on_shutdown(move || called2.set(called2.get() + 1));
        });
    });
    assert_eq!(called.get(), 2);
}

fn use_storage() {
    set_item(Value(1));
    let _ = get_item::<Value>();
    with_item::<Value, _, _>(|v| v.0);
    Arbiter::get_value(|| Value(2));
}

struct UseOnDrop;

impl Drop for UseOnDrop {
    fn drop(&mut self) {
        use_storage();
    }
}

thread_local!(static HOLD: RefCell<Option<UseOnDrop>> = const { RefCell::new(None) });

/// STATEMENT: Arbiter storage remains safe to access during thread-local destruction.
///
/// HOW:
/// - Access storage from its own stored destructor.
/// - Repeat from another thread-local destructor with both initialization orders.
#[test]
fn storage_access_during_thread_exit() {
    // item stored in STORAGE accesses STORAGE while it is destroyed
    thread::spawn(|| set_item(UseOnDrop)).join().unwrap();

    // other thread local accesses STORAGE, in both destruction orders
    thread::spawn(|| {
        HOLD.with(|h| *h.borrow_mut() = Some(UseOnDrop));
        use_storage();
    })
    .join()
    .unwrap();
    thread::spawn(|| {
        use_storage();
        HOLD.with(|h| *h.borrow_mut() = Some(UseOnDrop));
    })
    .join()
    .unwrap();
}

/// STATEMENT: A value borrowed through `with_item` survives replacement and storage removal.
///
/// HOW:
/// - Replace and clear storage inside the callback while reading the original allocation.
/// - Verify the original data remains readable and subsequent access sees empty storage.
#[test]
fn with_item_value_outlives_replacement() {
    #[derive(Clone, Default)]
    struct Item(std::rc::Rc<Vec<u8>>);

    thread::spawn(|| {
        set_item(Item(std::rc::Rc::new(vec![1; 64])));
        let len = with_item::<Item, _, _>(|item| {
            // replaces and frees the stored value while `f` holds its clone
            set_item(Item::default());
            unsafe { remove_all_items() };
            item.0.len()
        });
        assert_eq!(len, 64);
        assert!(get_item::<Item>().is_none());
        assert_eq!(with_item::<Item, _, _>(|item| item.0.len()), 0);
    })
    .join()
    .unwrap();
}

/// STATEMENT: Clearing storage permits reentrant destructors without retaining their inserts.
///
/// HOW:
/// - Store a value whose destructor reads storage and inserts another value.
/// - Clear storage and confirm neither the original nor reentrant values remain.
#[test]
fn remove_all_items_drops_outside_borrow() {
    struct Item;

    impl Drop for Item {
        fn drop(&mut self) {
            let _ = get_item::<u32>();
            set_item(2u64);
        }
    }

    thread::spawn(|| {
        set_item(Item);
        set_item(1u32);
        unsafe { remove_all_items() };
        assert!(get_item::<u32>().is_none());
        assert!(get_item::<u64>().is_none(), "item inserted by a destructor");
    })
    .join()
    .unwrap();
}

/// STATEMENT: Storage access during teardown returns fallbacks without caching new values.
///
/// HOW:
/// - In a stored destructor, try insertion, lookup, scoped access, and two lazy factories.
/// - Verify inserts are ignored and defaults or fresh factory results are returned.
#[test]
fn storage_fallback_values() {
    struct Check;

    impl Drop for Check {
        fn drop(&mut self) {
            set_item(Value(5));
            assert!(get_item::<Value>().is_none());
            assert_eq!(with_item::<Value, _, _>(|v| v.0), 0);
            assert_eq!(Arbiter::get_value(|| Value(3)).0, 3);
            assert_eq!(Arbiter::get_value(|| Value(4)).0, 4);
        }
    }
    thread::spawn(|| set_item(Check)).join().unwrap();
}
