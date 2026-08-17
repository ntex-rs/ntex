#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(any(feature = "tokio", feature = "neon"))]
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
#[cfg(any(feature = "tokio", feature = "neon"))]
use std::{io, sync::Arc};
use std::{net, sync::mpsc, thread, time};

use ntex::server::{TestServer, build};
use ntex::service::fn_service;
#[cfg(unix)]
use ntex::{SharedCfg, codec::BytesCodec, io::Io, util::Bytes};

#[test]
fn test_bind() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        sys.run(move || {
            let srv = build()
                .workers(1)
                .disable_signals()
                .bind("test", addr, SharedCfg::default(), async move || {
                    async |_| Ok::<_, ()>(())
                })
                .unwrap()
                .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (_, sys) = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(300));
    assert!(net::TcpStream::connect(addr).is_ok());
    sys.stop();
    let _ = h.join();
}

#[ntex::test]
async fn test_listen() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let lst = net::TcpListener::bind(addr).unwrap();
        let _ = sys.run(move || {
            let srv = build()
                .disable_signals()
                .workers(1)
                .listen("test", lst, SharedCfg::default(), async move || {
                    async |_| Ok::<_, ()>(())
                })
                .unwrap()
                .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        });
    });
    let (srv, sys) = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());

    srv.stop(true).await;
    sys.stop();
    let _ = h.join();
}

#[ntex::test]
#[cfg(unix)]
#[allow(clippy::unused_io_amount)]
async fn test_run() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        sys.run(move || {
            let srv = build()
                .backlog(100)
                .workers(1)
                .disable_signals()
                .bind("test", addr, SharedCfg::new("SRV"), async move || {
                    async move |io: Io| {
                        let _ = io.send(Bytes::from_static(b"test"), &BytesCodec).await;
                        Ok::<_, ()>(())
                    }
                })
                .unwrap()
                .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (srv, sys) = rx.recv().unwrap();

    let mut buf = [1u8; 4];
    let mut conn = net::TcpStream::connect(addr).unwrap();
    conn.write(&b"test"[..]).unwrap();
    let _ = conn.read_exact(&mut buf);
    assert_eq!(buf, b"test"[..]);

    // pause
    srv.pause().await;
    thread::sleep(time::Duration::from_millis(200));
    let mut conn = net::TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(time::Duration::from_millis(100)))
        .unwrap();
    let res = conn.read_exact(&mut buf);
    assert!(res.is_err());

    // resume
    srv.resume().await;
    thread::sleep(time::Duration::from_millis(100));
    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());

    let mut buf = [0u8; 4];
    let mut conn = net::TcpStream::connect(addr).unwrap();
    let _ = conn.read_exact(&mut buf);
    assert_eq!(buf, b"test"[..]);

    // stop
    srv.stop(false).await;
    thread::sleep(time::Duration::from_millis(100));
    assert!(net::TcpStream::connect(addr).is_err());

    thread::sleep(time::Duration::from_millis(100));
    sys.stop();
    let _ = h.join();
}

#[test]
#[cfg(feature = "tokio")]
fn test_on_worker_start() {
    let addr1 = TestServer::unused_addr();
    let addr2 = TestServer::unused_addr();
    let addr3 = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let h = thread::spawn(move || {
        let num = num2.clone();
        let num2 = num2.clone();
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let _ = sys.run(move || {
            let num = num.clone();
            ntex::rt::spawn(async move {
                let num = num.clone();
                let srv = build()
                    .disable_signals()
                    .configure(async move |cfg| {
                        let num = num.clone();
                        let lst = net::TcpListener::bind(addr3).unwrap();
                        cfg.bind("addr1", addr1, SharedCfg::default())
                            .unwrap()
                            .bind("addr2", addr2, SharedCfg::default())
                            .unwrap()
                            .listen("addr3", lst, SharedCfg::default())
                            .on_worker_start(async move |rt| {
                                let num = num.clone();
                                rt.service("addr1", async |_| Ok::<_, ()>(()));
                                rt.service("addr3", async |_| Ok::<_, ()>(()));
                                let _ = num.fetch_add(1, Relaxed);
                                Ok::<_, &'static str>(())
                            });
                        Ok::<_, io::Error>(())
                    })
                    .await
                    .unwrap()
                    .on_worker_start(async move || {
                        let _ = num2.fetch_add(1, Relaxed);
                        Ok::<_, &'static str>(())
                    })
                    .workers(1)
                    .run();
                let _ = tx.send((srv, ntex::rt::System::current()));
            });
            Ok::<_, io::Error>(())
        });
    });
    let (_, sys) = rx.recv().unwrap();
    thread::sleep(time::Duration::from_millis(500));

    assert!(net::TcpStream::connect(addr1).is_ok());
    assert!(net::TcpStream::connect(addr2).is_ok());
    assert!(net::TcpStream::connect(addr3).is_ok());
    assert_eq!(num.load(Relaxed), 2);
    sys.stop();
    let _ = h.join();
}

#[test]
#[cfg(feature = "tokio")]
fn test_configure_async() {
    let addr1 = TestServer::unused_addr();
    let addr2 = TestServer::unused_addr();
    let addr3 = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let h = thread::spawn(move || {
        let num = num2.clone();
        let num2 = num2.clone();
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let _ = sys.run(move || {
            ntex_rt::spawn(async move {
                let srv = build()
                    .disable_signals()
                    .configure(async move |cfg| {
                        let num = num.clone();
                        let lst = net::TcpListener::bind(addr3).unwrap();
                        cfg.bind("addr1", addr1, SharedCfg::new("srv-addr1"))
                            .unwrap()
                            .bind("addr2", addr2, SharedCfg::default())
                            .unwrap()
                            .listen("addr3", lst, SharedCfg::default())
                            .on_worker_start(async move |rt| {
                                assert!(format!("{:?}", rt).contains("ServiceRuntime"));
                                let num = num.clone();
                                rt.service("addr1", fn_service(async |_| Ok::<_, ()>(())));
                                rt.service("addr3", fn_service(async |_| Ok::<_, ()>(())));
                                let _ = num.fetch_add(1, Relaxed);
                                Ok::<_, &'static str>(())
                            });
                        Ok::<_, io::Error>(())
                    })
                    .await
                    .unwrap()
                    .on_worker_start(async move || {
                        let _ = num2.fetch_add(1, Relaxed);
                        Ok::<_, &'static str>(())
                    })
                    .workers(1)
                    .run();
                let _ = tx.send((srv.clone(), ntex::rt::System::current()));
                let _ = srv.await;
            });
            Ok::<_, io::Error>(())
        });
    });
    let (_, sys) = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(500));

    assert!(net::TcpStream::connect(addr1).is_ok());
    assert!(net::TcpStream::connect(addr2).is_ok());
    assert!(net::TcpStream::connect(addr3).is_ok());
    assert_eq!(num.load(Relaxed), 2);
    sys.stop();
    let _ = h.join();
}

#[test]
#[cfg(feature = "tokio")]
#[allow(unreachable_code)]
fn test_panic_in_worker() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter2 = counter.clone();

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        let counter = counter2.clone();
        sys.run(move || {
            let counter = counter.clone();
            let srv = build()
                .workers(1)
                .disable_signals()
                .bind("test", addr, SharedCfg::default(), async move || {
                    let counter = counter.clone();
                    async move |_| {
                        counter.fetch_add(1, Relaxed);
                        panic!();
                        Ok::<_, ()>(())
                    }
                })
                .unwrap()
                .run();
            let _ = tx.send((srv.clone(), ntex::rt::System::current()));
            ntex::rt::spawn(async move {
                let _ = srv.await;
            });
            Ok(())
        })
    });
    let (_, sys) = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(200));
    assert!(net::TcpStream::connect(addr).is_ok());
    thread::sleep(time::Duration::from_millis(100));
    assert_eq!(counter.load(Relaxed), 1);

    // first connect get dropped, because there is no workers
    assert!(net::TcpStream::connect(addr).is_ok());
    thread::sleep(time::Duration::from_millis(300));
    assert!(net::TcpStream::connect(addr).is_ok());
    thread::sleep(time::Duration::from_millis(500));
    assert_eq!(counter.load(Relaxed), 3);

    sys.stop();
    let _ = h.join();
}

#[test]
#[cfg(feature = "neon")]
fn test_on_accept() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let h = thread::spawn(move || {
        let num = num2.clone();
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        sys.run(move || {
            let srv = build()
                .disable_signals()
                .bind("test", addr, SharedCfg::default(), async move || {
                    async move |io: Io| {
                        let _ = io.send(Bytes::from_static(b"test"), &BytesCodec).await;
                        Ok::<_, ()>(())
                    }
                })
                .unwrap()
                .on_accept(async move |name, io| {
                    if name.as_ref() == "test" {
                        let _ = num.fetch_add(1, Relaxed);
                    }
                    Ok::<_, io::Error>(io)
                })
                .workers(1)
                .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
    });
    let (_, sys) = rx.recv().unwrap();

    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());
    thread::sleep(time::Duration::from_millis(250));
    assert_eq!(num.load(Relaxed), 3);
    sys.stop();
    let _ = h.join();
}

#[cfg(not(any(feature = "tokio", feature = "compio")))]
#[test]
fn test_stop_on_panic() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();
    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test", ntex::rt::DefaultRuntime);
        sys.block_on(async move {
            let _ = build()
                .stop_on_panic()
                .graceful_shutdown()
                .workers(1)
                .disable_signals()
                .bind("test", addr, SharedCfg::default(), async move || {
                    #[allow(unreachable_code)]
                    fn_service(async move |_| {
                        panic!("test");
                        Ok::<_, ()>(())
                    })
                })
                .unwrap()
                .run()
                .await;
            let _ = tx.send("test");
        });
    });

    thread::sleep(time::Duration::from_millis(300));
    assert!(net::TcpStream::connect(addr).is_ok());

    let res = rx.recv().unwrap();
    assert_eq!(res, "test");
    let _ = h.join();
}
