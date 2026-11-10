use std::sync::atomic::{AtomicUsize, Ordering};
use std::{sync::mpsc, thread};

use ntex::rt::{self, Arbiter, Handle, System};
use ntex::time::{Millis, sleep};

/// STATEMENT: Local and remote runtime join handles return task results across threads.
///
/// HOW:
/// - Spawn local and same-thread remote tasks and assert their values.
/// - Submit a task through `Handle` from another thread, join it there, and assert its result and the `Arbiter` debug label.
#[ntex::test]
async fn test_join_handle() {
    let hnd = rt::Handle::current();
    let f: rt::JoinHandle<_> = rt::spawn(async { std::rc::Rc::new("test") });
    assert_eq!(*f.await.unwrap(), "test");
    let local: rt::RemoteJoinHandle<_> = hnd.spawn(async { "local" });
    assert_eq!(local.await.unwrap(), "local");
    let (tx, rx) = oneshot::async_channel();

    let thread = thread::spawn(move || {
        let runner = crate::System::build().build(ntex::rt::DefaultRuntime);
        // submitted to the test's arbiter, joined on this thread
        let remote: rt::RemoteJoinHandle<_> = hnd.spawn(async { "test2" });
        let result = runner.block_on(remote).unwrap();
        let _ = tx.send(result);
    });

    let result = rx.await.unwrap();
    assert_eq!(result, "test2");
    thread.join().unwrap();

    assert!(format!("{:?}", Arbiter::current()).contains("Arbiter"));
}

/// STATEMENT: System and arbiter handles execute tasks in their owning system.
///
/// HOW:
/// - Run a system on another thread and submit detached, boxed, and joined tasks through its handles.
/// - Compare each task's `System::current().id()` with the target system ID.
#[test]
fn test_async() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let runner = crate::System::build().build(ntex::rt::DefaultRuntime);

        let _ = runner.run(move || {
            tx.send(System::current()).unwrap();
            Ok(())
        });
    });
    let s = System::new("test", ntex_net::DefaultRuntime);

    let sys = rx.recv().unwrap();
    let id = sys.id();
    let (tx, rx) = mpsc::channel();
    sys.handle().spawn_detached(async move {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    sys.handle().spawn_detached(async move {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    sys.arbiter().handle().spawn_detached(async move {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let task = sys.handle().spawn(async { System::current().id() });
    let id2 = s.block_on(task).unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    sys.handle().spawn_detached(Box::pin(async move {
        let _ = tx.send(System::current().id());
    }));
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);
}

/// STATEMENT: A custom Tokio runner preserves system context and records arbiter pings.
///
/// HOW:
/// - Implement `Runner` with a current-thread Tokio runtime and run a system with a short ping interval.
/// - Assert submitted tasks observe the owning system ID and ping records are nonempty before stopping.
#[cfg(feature = "tokio")]
#[test]
fn test_block_on() {
    let (tx, rx) = mpsc::channel();

    struct Custom;

    impl ntex::rt::Runner for Custom {
        fn block_on(
            &self,
            fut: ntex::rt::BlockFuture,
        ) -> Result<(), Box<dyn std::any::Any + Send>> {
            let rt = ntex::rt::tokio::internal::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            ntex::rt::tokio::internal::task::LocalSet::new().block_on(&rt, fut);
            Ok(())
        }
    }

    thread::spawn(move || {
        let runner = crate::System::build().ping_interval(25).build(Custom);

        let _ = runner.run(move || {
            tx.send(System::current()).unwrap();
            Ok(())
        });
    });
    let s = System::new("test", ntex::rt::DefaultRuntime);

    let sys = rx.recv().unwrap();
    let id = sys.id();
    let (tx, rx) = mpsc::channel();
    sys.arbiter().handle().spawn_detached(async move {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let task = sys.handle().spawn(async { System::current().id() });
    let id2 = s.block_on(task).unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    // timers are thread-local, create the sleep on the system thread
    sys.handle().spawn_detached(async move {
        rt::spawn_detached(async move {
            ntex::time::sleep(std::time::Duration::from_millis(100)).await;
            System::list_arbiter_pings(|_, recs| tx.send(recs.clone()).unwrap());
        });
    });
    let recs = rx.recv().unwrap();

    assert!(!recs.is_empty());
    sys.stop();
}

/// STATEMENT: Arbiter-local and runtime typed storage return initialized values.
///
/// HOW:
/// - Create a `System`, initialize an arbiter-local `u64`, and assert it is `64`.
/// - Store `100u32` with `set_item` and retrieve the same value with `get_item`.
#[test]
fn test_arbiter_local_storage() {
    let _s = System::new("test", ntex::rt::DefaultRuntime);
    assert!(Arbiter::get_value(|| 64u64) == 64);

    ntex::rt::set_item(100u32);
    assert_eq!(ntex::rt::get_item::<u32>().unwrap(), 100);
}

/// STATEMENT: Spawned tasks expose completion and return their values through handles.
///
/// HOW:
/// - Spawn a sleeping task and check `is_finished` before and after awaiting it.
/// - Spawn through the current `Handle` and assert the joined result is `1`.
#[test]
fn test_spawn_api() {
    System::new("test", ntex::rt::DefaultRuntime).block_on(async {
        let mut hnd = ntex::rt::spawn(async {
            sleep(Millis(25)).await;
        });
        assert!(!hnd.is_finished());
        let _ = (&mut hnd).await;
        assert!(hnd.is_finished());

        let hnd = crate::Handle::current();
        let task = hnd.spawn(async move { 1 });
        let res = task.await.unwrap();
        assert_eq!(res, 1);
    });
}

static CB_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn counting_callbacks() -> ntex::rt::TaskCallbacks {
    ntex::rt::TaskCallbacks {
        before: || {
            CB_COUNTER.fetch_add(1, Ordering::Relaxed);
            Some(CB_COUNTER.as_ptr().cast_const().cast())
        },
        enter: |ptr| {
            CB_COUNTER.fetch_add(1, Ordering::Relaxed);
            ptr
        },
        exit: |_| {
            CB_COUNTER.fetch_add(1, Ordering::Relaxed);
        },
        after: |_| {
            CB_COUNTER.fetch_add(1, Ordering::Relaxed);
        },
    }
}

/// STATEMENT: Task callbacks set with the `task_callbacks` test argument run for spawned work.
///
/// HOW:
/// - Run the test system with counting callbacks passed through `#[ntex::test]`.
/// - Join a sleeping task, assert completion, and verify `before`, `enter`,
///   `exit`, and `after` increased the counter.
#[ntex::test(task_callbacks = counting_callbacks)]
async fn test_spawn_cb() {
    let start = CB_COUNTER.load(Ordering::Relaxed);
    let mut hnd = ntex::rt::spawn(async {
        sleep(Millis(25)).await;
    });
    let _ = (&mut hnd).await;
    assert!(hnd.is_finished());

    // `before` and `after` once, `enter` and `exit` for each of at least two polls
    assert!(CB_COUNTER.load(Ordering::Relaxed) - start >= 6);
}

/// STATEMENT: `Arbiter::on_shutdown` callbacks, including nested registrations, run while the arbiter is still running.
///
/// HOW:
/// - Register a shutdown callback that registers another callback on a new arbiter.
/// - Stop the arbiter, assert the nested callback observes `is_running()`, and join the arbiter.
#[test]
fn test_on_shutdown_arbiter() {
    System::new("test", ntex::rt::DefaultRuntime).block_on(async {
        let (tx, rx) = oneshot::channel();
        let mut arb = Arbiter::new();
        let registered = arb.handle().spawn(async move {
            Arbiter::on_shutdown(move || {
                Arbiter::on_shutdown(move || {
                    let _ = tx.send(Arbiter::current().is_running());
                });
            });
        });
        registered.await.unwrap();

        arb.stop();
        assert!(
            rx.await.unwrap(),
            "callback ran after the event loop exited"
        );
        arb.join().unwrap();
    });
}

/// STATEMENT: System shutdown callbacks run on explicit stop and on event-loop exit without a stop request.
///
/// HOW:
/// - Register a counting callback in a system that calls `System::stop` and assert one invocation.
/// - Register another in `block_on`, assert it has not run inside the loop, and assert one additional invocation on exit.
#[test]
fn test_on_shutdown_system() {
    thread::spawn(|| {
        let ran = std::rc::Rc::new(std::cell::Cell::new(0));

        let ran2 = ran.clone();
        System::build()
            .build(ntex::rt::DefaultRuntime)
            .run(move || {
                Arbiter::on_shutdown(move || ran2.set(ran2.get() + 1));
                System::current().stop();
                Ok(())
            })
            .unwrap();
        assert_eq!(ran.get(), 1);

        let ran2 = ran.clone();
        System::new("test", ntex::rt::DefaultRuntime).block_on(async move {
            let ran3 = ran2.clone();
            Arbiter::on_shutdown(move || ran3.set(ran3.get() + 1));
            sleep(Millis(10)).await;
            assert_eq!(ran2.get(), 1, "callback ran before shutdown");
        });
        assert_eq!(ran.get(), 2);
    })
    .join()
    .unwrap();
}

/// STATEMENT: System-scoped typed storage is shared across arbiters.
///
/// HOW:
/// - Initialize a `usize` to `10` in the current system.
/// - Read it from a new arbiter using an initializer returning `11` and assert the existing value `10` is retained.
#[ntex::test]
async fn system_storage() {
    let val = System::current().get_value::<usize>(|| 10);
    assert_eq!(val, 10);

    let arb = Arbiter::new();
    let task = arb
        .handle()
        .spawn(async { System::current().get_value::<usize>(|| 11) });
    let val2 = task.await.unwrap();
    assert_eq!(val2, 10);
}

/// STATEMENT: The io-uring backend drains input and observes EOF after an idle peer closes.
///
/// HOW:
/// - Use small read buffers and backpressure while a server sends two delayed copies of `DATA` and closes.
/// - Receive with `BytesCodec` under a five-second timeout and assert EOF arrives after exactly twice the payload length.
#[cfg(all(target_os = "linux", feature = "neon-uring"))]
#[ntex::test]
async fn idle_disconnect_uring() {
    use std::sync::{Arc, Mutex};

    use ntex::io::{Io, IoConfig};
    use ntex::util::{BytePageSize, Bytes};
    use ntex::{SharedCfg, codec::BytesCodec, connect::Connect};
    use ntex::{time::Millis, time::sleep, time::timeout};

    const DATA: &[u8] = b"Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World \
                          Hello World Hello World Hello World Hello World Hello World";

    let (tx, rx) = ::oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let server = ntex::server::test_server(async move |_| {
        let tx = tx.clone();
        ntex::fn_service(move |io: Io<_>| {
            tx.lock().unwrap().take().unwrap().send(()).unwrap();

            async move {
                io.encode_slice(DATA).unwrap();
                sleep(Millis(250)).await;
                io.encode_bytes(Bytes::from_static(DATA)).unwrap();
                sleep(Millis(250)).await;
                io.close();
                Ok::<_, ()>(())
            }
        })
    });

    let cfg = SharedCfg::new("NEON-URING")
        .add(
            IoConfig::new()
                .set_read_size(BytePageSize::Size4, BytePageSize::Size4)
                .set_read_backpressure(BytePageSize::Size4.capacity()),
        )
        .build();

    let msg = Connect::new(server.addr());
    let io = ntex::connect::connect_with(msg, &cfg).await.unwrap();
    rx.await.unwrap();

    // Server close is a FIN, which is indistinguishable from a half-close, so
    // it is observed as eof once all input is read
    let mut total = 0;
    let res = timeout(Millis(5000), async {
        while let Ok(Some(item)) = io.recv(&BytesCodec).await {
            total += item.len();
        }
    })
    .await;
    assert!(res.is_ok(), "eof is not observed");
    assert_eq!(total, DATA.len() * 2);
}

/// STATEMENT: Arbiter latency callbacks report stalls and supply the spawn location when available.
///
/// HOW:
/// - Run a deliberately blocking arbiter once without a callback, then repeat with `System::set_latency_callback` installed.
/// - Assert the callback ran and, when a location is reported, that its file is this test file.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn test_log_backtrace() {
    use std::{sync::Arc, sync::atomic::AtomicBool, time::Duration};

    fn run(tx: oneshot::Sender<()>) {
        thread::spawn(move || {
            crate::System::build()
                .signals(true)
                .ping_interval(1000)
                .ping_threshold(500)
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let (tx, rx) = oneshot::channel();
                    Arbiter::with_name("ttttt".to_string())
                        .handle()
                        .spawn(async {
                            thread::sleep(Duration::from_secs(2));
                            let _ = tx.send(());
                        });

                    let _ = rx.await;
                });
            let _ = tx.send(());
        });
    }

    let (tx, rx) = oneshot::channel();
    run(tx);
    let _ = rx.recv();

    let tag = Arc::new(AtomicBool::new(false));
    let tag2 = tag.clone();
    let task = Arc::new(std::sync::Mutex::new(None));
    let task2 = task.clone();
    System::set_latency_callback(move |_, loc| {
        *task2.lock().unwrap() = loc;
        tag2.store(true, Ordering::Relaxed);
    });

    let (tx, rx) = oneshot::channel();
    run(tx);
    let _ = rx.recv();
    assert!(tag.load(Ordering::Relaxed));
    // neon reports the spawn location, tokio and compio report none
    if let Some(loc) = *task.lock().unwrap() {
        assert_eq!(loc.file(), file!());
    }
}
