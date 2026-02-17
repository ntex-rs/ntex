#![allow(deprecated)]
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::{sync::mpsc, thread};

use ntex::rt::{self, Arbiter, Handle, System};
use ntex::time::{Millis, sleep};

#[ntex::test]
async fn test_join_handle() {
    fn __assert_send<Fut, T>(f: Fut) -> Fut
    where
        Fut: Future<Output = T> + Send,
    {
        f
    }

    let hnd = rt::Handle::current();
    let f = rt::spawn(async { "test" });
    let f = __assert_send(f);
    let (tx, rx) = oneshot::channel();

    thread::spawn(move || {
        let runner = crate::System::build()
            .stop_on_panic(true)
            .build(ntex::rt::DefaultRuntime);
        let result = runner.block_on(f).unwrap();
        assert_eq!(result, "test");

        let runner = crate::System::build()
            .stop_on_panic(true)
            .build(ntex::rt::DefaultRuntime);
        let result = runner.block_on(hnd.spawn(async { "test2" })).unwrap();
        let _ = tx.send(result);
    });

    let result = rx.await.unwrap();
    assert_eq!(result, "test2");

    assert!(format!("{:?}", Arbiter::current()).contains("Arbiter"));
}

#[test]
fn test_async() {
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let runner = crate::System::build()
            .stop_on_panic(true)
            .build(ntex::rt::DefaultRuntime);

        let _ = runner.run(move || {
            tx.send(System::current()).unwrap();
            Ok(())
        });
    });
    let s = System::new("test", ntex_net::DefaultRuntime);

    let sys = rx.recv().unwrap();
    let id = sys.id();
    let (tx, rx) = mpsc::channel();
    sys.arbiter().exec_fn(move || {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    sys.handle().spawn(async move {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    sys.arbiter().handle().spawn(async move {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let id2 = s
        .block_on(sys.arbiter().exec(|| System::current().id()))
        .unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    sys.arbiter().spawn(Box::pin(async move {
        let _ = tx.send(System::current().id());
    }));
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);
}

#[cfg(feature = "tokio")]
#[test]
fn test_block_on() {
    let (tx, rx) = mpsc::channel();

    struct Custom;

    impl ntex::rt::Runner for Custom {
        fn block_on(&self, fut: ntex::rt::BlockFuture) {
            let rt = ntex::rt::tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            ntex::rt::tokio::task::LocalSet::new().block_on(&rt, fut);
        }
    }

    thread::spawn(move || {
        let runner = crate::System::build()
            .stop_on_panic(true)
            .ping_interval(25)
            .build(Custom);

        let _ = runner.run(move || {
            tx.send(System::current()).unwrap();
            Ok(())
        });
    });
    let s = System::new("test", ntex::rt::DefaultRuntime);

    let sys = rx.recv().unwrap();
    let id = sys.id();
    let (tx, rx) = mpsc::channel();
    sys.arbiter().exec_fn(move || {
        let _ = tx.send(System::current().id());
    });
    let id2 = rx.recv().unwrap();
    assert_eq!(id, id2);

    let id2 = s
        .block_on(sys.arbiter().exec(|| System::current().id()))
        .unwrap();
    assert_eq!(id, id2);

    let (tx, rx) = mpsc::channel();
    sys.arbiter().spawn(async move {
        ntex::time::sleep(std::time::Duration::from_millis(100)).await;

        let recs = System::list_arbiter_pings(Arbiter::current().id(), |recs| {
            recs.unwrap().clone()
        });
        let _ = tx.send(recs);
    });
    let recs = rx.recv().unwrap();

    assert!(!recs.is_empty());
    sys.stop();
}

#[test]
fn test_arbiter_local_storage() {
    let _s = System::new("test", ntex::rt::DefaultRuntime);
    Arbiter::set_item("test");
    assert!(Arbiter::get_item::<&'static str, _, _>(|s| *s == "test"));
    assert!(Arbiter::contains_item::<&'static str>());
    assert!(Arbiter::get_value(|| 64u64) == 64);

    ntex::rt::set_item(100u32);
    assert!(ntex::rt::get_item::<u32, _, _>(|s| *s.unwrap() == 100));
}

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
        let res = hnd
            .spawn(async {
                sleep(Millis(25)).await;
                1
            })
            .await
            .unwrap();
        assert_eq!(res, 1);
    });
}

#[test]
fn test_spawn_cb() {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    let before = move || {
        let _ = c.fetch_add(1, Ordering::Relaxed);
        Some(c.as_ptr() as *const _)
    };
    let c = counter.clone();
    let enter = move |_| {
        let _ = c.fetch_add(1, Ordering::Relaxed);
        c.as_ptr() as *const _
    };
    let c = counter.clone();
    let exit = move |_| {
        let _ = c.fetch_add(1, Ordering::Relaxed);
    };
    let c = counter.clone();
    let after = move |_| {
        let _ = c.fetch_add(1, Ordering::Relaxed);
    };

    unsafe {
        let set = ntex::rt::task_opt_callbacks(
            before.clone(),
            enter.clone(),
            exit.clone(),
            after.clone(),
        );
        assert!(set);
        let set = ntex::rt::task_opt_callbacks(before, enter, exit, after);
        assert!(!set);
    }

    System::new("test", ntex::rt::DefaultRuntime).block_on(async {
        let mut hnd = ntex::rt::spawn(async {
            sleep(Millis(25)).await;
        });
        let _ = (&mut hnd).await;
        assert!(hnd.is_finished());
    });

    let val = counter.load(Ordering::Relaxed);
    assert!(val > 0);
}

#[cfg(all(target_os = "linux", feature = "neon-polling"))]
#[ntex::test]
async fn idle_disconnect_polling() {
    use std::sync::Mutex;

    use ntex::connect::Connect;
    use ntex::{SharedCfg, io::Io, io::IoConfig, time::Millis, time::sleep};

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

    let server = ntex::server::test_server(async move || {
        let tx = tx.clone();
        ntex::fn_service(move |io: Io<_>| {
            tx.lock().unwrap().take().unwrap().send(()).unwrap();

            async move {
                io.write(DATA).unwrap();
                sleep(Millis(250)).await;
                io.close();
                Ok::<_, ()>(())
            }
        })
    });

    let cfg = SharedCfg::new("NEON")
        .add(IoConfig::new().set_read_buf(24, 12, 16))
        .into();

    let msg = Connect::new(server.addr());
    let io = ntex::connect::connect_with(msg, cfg).await.unwrap();
    rx.await.unwrap();

    io.on_disconnect().await;
}

#[cfg(all(target_os = "linux", feature = "neon-uring"))]
#[ntex::test]
async fn idle_disconnect_uring() {
    use std::sync::Mutex;

    use ntex::{
        SharedCfg, connect::Connect, io::Io, io::IoConfig, time::Millis, time::sleep,
    };

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

    let server = ntex::server::test_server(async move || {
        let tx = tx.clone();
        ntex::fn_service(move |io: Io<_>| {
            tx.lock().unwrap().take().unwrap().send(()).unwrap();

            async move {
                io.write(DATA).unwrap();
                sleep(Millis(250)).await;
                io.write(DATA).unwrap();
                sleep(Millis(250)).await;
                io.close();
                Ok::<_, ()>(())
            }
        })
    });

    let cfg = SharedCfg::new("NEON-URING")
        .add(IoConfig::new().set_read_buf(24, 12, 16))
        .into();

    let msg = Connect::new(server.addr());
    let io = ntex::connect::connect_with(msg, cfg).await.unwrap();
    rx.await.unwrap();

    io.on_disconnect().await;
}
