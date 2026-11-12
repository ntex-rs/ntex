//! Network server integration tests.
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::{net, time::Duration};

use ntex::codec::BytesCodec;
use ntex::time::{Millis, sleep};
use ntex::util::Bytes;
use ntex_io::{Io, IoConfig};
use ntex_server::net::{
    AcceptLoop, AcceptorCommand, Affinity, LoadBalancing, ServerBuilder, ServerStatus, TestServer,
    TestServerBuilder, bind_addr, build, build_test_server, build_with_config, create_tcp_listener,
    test_server,
};
use ntex_service::{Ctx, Service, cfg::SharedCfg, fn_service};

async fn echo(io: Io) -> Result<(), ()> {
    while let Ok(Some(msg)) = io.recv(&BytesCodec).await {
        if io.send(msg, &BytesCodec).await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn write_tag(io: Io, tag: &'static [u8]) -> Result<(), ()> {
    let _ = io.send(Bytes::from_static(tag), &BytesCodec).await;
    Ok(())
}

async fn check_echo(io: &Io) {
    io.send(Bytes::from_static(b"hello"), &BytesCodec)
        .await
        .unwrap();
    let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(msg, Bytes::from_static(b"hello"));
}

/// Reads the tag written by `write_tag`.
fn read_tag(addr: net::SocketAddr) -> io::Result<Vec<u8>> {
    let mut conn = net::TcpStream::connect(addr)?;
    conn.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut buf = Vec::new();
    conn.read_to_end(&mut buf)?;
    Ok(buf)
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

fn local_listener() -> (net::TcpListener, net::SocketAddr) {
    let lst = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lst.local_addr().unwrap();
    (lst, addr)
}

#[ntex::test]
async fn test_server_echo() {
    let srv = test_server(async |_| fn_service(echo));
    assert!(format!("{srv:?}").contains("TestServer"));
    assert_eq!(srv.config().tag(), "TEST-CLIENT");

    let io = srv.connect().await.unwrap();
    check_echo(&io).await;
    assert_eq!(io.cfg().tag(), "TEST-CLIENT");

    let srv2 = srv.clone();
    assert_eq!(srv2.addr(), srv.addr());
    srv.stop();
    ntex::time::timeout(Millis(5000), srv.server())
        .await
        .unwrap()
        .unwrap();
    assert!(net::TcpStream::connect(srv.addr()).is_err());
}

/// Connections with a known incoming CPU are counted as hits or misses.
#[ntex::test]
async fn test_incoming_cpu_connections() {
    let (lst, addr) = local_listener();
    let srv = build()
        .workers(2)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    sleep(Millis(100)).await;

    let mut conns = Vec::new();
    for _ in 0..4 {
        let io = ntex_net::tcp_connect(addr, SharedCfg::default())
            .await
            .unwrap();
        check_echo(&io).await;
        conns.push(io);
    }
    let m = srv.metrics();
    let counted = m.incoming_cpu_hits + m.incoming_cpu_misses;
    // loopback connections have an incoming CPU on Linux, but no RSS CPU
    // on Windows
    if cfg!(any(target_os = "linux", target_os = "android")) {
        assert_eq!(counted, 4);
    } else {
        assert_eq!(counted, 0);
    }
    srv.stop(false).await;
}

#[ntex::test]
async fn test_least_loaded_connections() {
    // the test server runs a single worker
    let (lst, addr) = local_listener();
    let srv = build()
        .workers(2)
        .load_balancing(LoadBalancing::LeastLoaded)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    let connect = || ntex_net::tcp_connect(addr, SharedCfg::default());
    let active = || {
        let mut conns: Vec<_> = srv
            .metrics()
            .workers
            .iter()
            .map(|w| w.active_connections)
            .collect();
        conns.sort_unstable();
        conns
    };
    sleep(Millis(100)).await;

    let io1 = connect().await.unwrap();
    check_echo(&io1).await;
    let io2 = connect().await.unwrap();
    check_echo(&io2).await;
    assert_eq!(active(), [1, 1]);

    // the next connection goes to the worker without connections,
    // round-robin would send it to the worker of `io1`
    io2.close();
    wait_for(|| active() == [0, 1]).await;
    let io3 = connect().await.unwrap();
    check_echo(&io3).await;
    assert_eq!(active(), [1, 1]);
    srv.stop(false).await;
}

#[ntex::test]
async fn test_worker_io_metrics() {
    const SIZE: usize = 8 * 1024 * 1024;

    let (lst, addr) = local_listener();
    let (lst2, addr2) = local_listener();
    let srv = build()
        .workers(1)
        .disable_signals()
        .listen(
            "test",
            lst,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(1024)),
            async |_: &()| {
                fn_service(async |io: Io| {
                    io.encode_slice(&vec![b'x'; SIZE]).unwrap();
                    let _ = io.flush(true).await;
                    let _ = io.recv(&BytesCodec).await;
                    Ok::<_, ()>(())
                })
            },
        )
        .unwrap()
        .listen("test2", lst2, SharedCfg::new("SRV2"), async |_: &()| {
            fn_service(async |io: Io| {
                let _ = io.recv(&BytesCodec).await;
                Ok::<_, ()>(())
            })
        })
        .unwrap()
        .run();
    let tag_metrics = |tag: &str| {
        srv.metrics().workers[0]
            .io
            .iter()
            .find(|m| m.tag == tag)
            .map(|m| m.io)
            .unwrap_or_default()
    };
    let io_metrics = || tag_metrics("SRV");
    sleep(Millis(100)).await;

    // the client does not read, the server is under write backpressure
    let io = ntex_net::tcp_connect(addr, SharedCfg::default())
        .await
        .unwrap();
    wait_for(|| io_metrics().write_backpressure_active == 1).await;
    let m = io_metrics();
    assert_eq!(m.write_backpressure, 1);
    assert!(m.write_buffered > 0);
    assert!(m.write_buffered <= SIZE);

    let mut received = 0;
    while received < SIZE {
        received += io.recv(&BytesCodec).await.unwrap().unwrap().len();
    }
    assert_eq!(received, SIZE);
    wait_for(|| io_metrics().write_backpressure_active == 0).await;
    let m = io_metrics();
    assert_eq!(m.write_backpressure, 1);
    assert!(m.write_backpressure_time > Duration::ZERO);
    assert_eq!(m.bytes_written, SIZE as u64);
    // io-uring always uses the write task
    assert!(m.eager_writes + m.scheduled_writes > 0);

    // the worker allocated and released write pages
    let pc = srv.metrics().workers[0].page_cache.total();
    assert!(pc.hits + pc.misses > 0);
    assert!(pc.cached > 0);
    assert_eq!(pc.bytes > 0, pc.pages > 0);

    io.encode_slice(b"hello").unwrap();
    io.flush(true).await.unwrap();
    wait_for(|| io_metrics().bytes_read == 5).await;

    io.close();
    wait_for(|| {
        let m = io_metrics();
        m.write_buffered == 0 && m.read_buffered == 0 && m.closed.total == 1
    })
    .await;
    // the service does not report a close reason
    assert_eq!(io_metrics().closed.other, 1);

    // each listener configuration has its own metrics
    let io = ntex_net::tcp_connect(addr2, SharedCfg::default())
        .await
        .unwrap();
    io.encode_slice(b"hi").unwrap();
    io.flush(true).await.unwrap();
    wait_for(|| tag_metrics("SRV2").bytes_read == 2).await;
    io.close();
    wait_for(|| tag_metrics("SRV2").closed.total == 1).await;
    assert_eq!(io_metrics().closed.total, 1);
    assert_eq!(io_metrics().bytes_read, 5);
    // listener configurations are registered, no connection is counted as other
    assert_eq!(srv.metrics().workers[0].io_other.closed.total, 0);
    srv.stop(false).await;
}

#[ntex::test]
async fn test_server_builder() {
    let (tx, rx) = mpsc::channel();
    let builder = TestServerBuilder::new(async move |_| {
        let tx = tx.clone();
        fn_service(async move |io: Io| {
            let _ = tx.send(io.cfg().tag().to_string());
            echo(io).await
        })
    })
    .config(SharedCfg::new("SRV"))
    .client_config(SharedCfg::new("CLIENT"));
    assert!(format!("{builder:?}").contains("TestServerBuilder"));

    let srv = builder.start();
    assert_eq!(srv.config().tag(), "CLIENT");
    let io = srv.connect().await.unwrap();
    check_echo(&io).await;
    assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), "SRV");
}

#[derive(Clone)]
struct St(Arc<AtomicUsize>);

#[ntex::test]
async fn test_server_with_state() {
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();
    let srv = TestServerBuilder::with(
        async move || Ok(St(num2.clone())),
        async |_| {
            async |st: &St, io: Io| {
                st.0.fetch_add(1, Relaxed);
                echo(io).await
            }
        },
    )
    .start();

    let io = srv.connect().await.unwrap();
    check_echo(&io).await;
    assert_eq!(num.load(Relaxed), 1);
}

/// STATEMENT: A test-server factory can use non-`Send` worker state across an
/// await and share an owned handle with the resulting service.
///
/// HOW:
/// - Initialize `Rc<Cell<u8>>` state to `40` with a zero-argument state factory.
/// - Await in the service factory, increment the state, and clone its handle.
/// - Assert that each call receives the same state and returns its incremented
///   value, `42` then `43`, over two connections.
#[ntex::test]
async fn test_server_factory_worker_state() {
    use std::{cell::Cell, rc::Rc};

    let srv = TestServerBuilder::with(
        async || Ok(Rc::new(Cell::new(40u8))),
        async |st| {
            sleep(Millis(1)).await;
            st.set(st.get() + 1);
            let initialized = st.clone();
            async move |st: &Rc<Cell<u8>>, io: Io| {
                assert!(Rc::ptr_eq(st, &initialized));
                st.set(st.get() + 1);
                io.send(Bytes::from(vec![st.get()]), &BytesCodec)
                    .await
                    .unwrap();
                echo(io).await
            }
        },
    )
    .start();

    for expected in [42, 43] {
        let io = srv.connect().await.unwrap();
        let response = ntex::time::timeout(Millis(5000), io.recv(&BytesCodec))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response.as_ref(), &[expected]);
        io.close();
    }
}

#[ntex::test]
async fn test_build_test_server() {
    let (lst, addr) = local_listener();
    let srv = build_test_server(
        async || Ok(St(Arc::new(AtomicUsize::new(0)))),
        async move |builder| {
            builder
                .name("test-srv")
                .backlog(64)
                .graceful_shutdown_timeout(Millis(1000))
                .listen("test", lst, SharedCfg::new("LISTEN"), async |st: &St| {
                    let st = st.clone();
                    fn_service(async move |io: Io| {
                        st.0.fetch_add(1, Relaxed);
                        echo(io).await
                    })
                })
                .unwrap()
        },
    )
    .set_addr(addr);
    assert_eq!(srv.addr(), addr);

    let io = srv.connect().await.unwrap();
    check_echo(&io).await;
}

#[ntex::test]
async fn test_configure() {
    let (lst, addr3) = local_listener();
    let addr1 = TestServer::unused_addr();
    let addr2 = TestServer::unused_addr();
    let addr4 = TestServer::unused_addr();
    let started = Arc::new(AtomicUsize::new(0));
    let started2 = started.clone();

    let srv = build_test_server(
        async || Ok(10usize),
        async move |builder: ServerBuilder<_>| {
            builder
                .bind("bind", addr4, SharedCfg::default(), async |_: &usize| {
                    fn_service(async |io| write_tag(io, b"bind").await)
                })
                .unwrap()
                .configure(async move |cfg| {
                    cfg.bind("addr1", addr1)?
                        .bind("addr2", addr2)?
                        .listen("addr3", lst)
                        .on_worker_start(async move |rt| {
                            started2.fetch_add(1, Relaxed);
                            assert_eq!(*rt.cfg(), 10);
                            assert!(format!("{rt:?}").contains("addr1"));
                            rt.service("addr1", SharedCfg::new("A1"), async |st: &usize, io| {
                                assert_eq!(*st, 10);
                                write_tag(io, b"addr1").await
                            });
                            // services with a different state
                            let rt2 = rt.map_cfg("state");
                            assert_eq!(*rt2.cfg(), "state");
                            rt2.service("addr3", SharedCfg::default(), async |st: &&str, io| {
                                assert_eq!(*st, "state");
                                write_tag(io, b"addr3").await
                            });
                            Ok(())
                        })
                        .on_worker_start(async |rt| {
                            // replaces the previous service
                            rt.service("addr1", SharedCfg::default(), async |_: &usize, io| {
                                write_tag(io, b"addr1-2").await
                            });
                            Ok(())
                        });
                    Ok(())
                })
                .await
                .unwrap()
        },
    );

    assert_eq!(read_tag(addr1).unwrap(), b"addr1-2");
    assert_eq!(read_tag(addr3).unwrap(), b"addr3");
    assert_eq!(read_tag(addr4).unwrap(), b"bind");
    // "addr2" has no service, connections are dropped
    assert_eq!(read_tag(addr2).map(|v| v.is_empty()).ok(), Some(true));
    assert_eq!(started.load(Relaxed), 1);
    drop(srv);
}

/// Listeners without `on_worker_start` are not served.
#[ntex::test]
async fn test_configure_without_worker_start() {
    let addr = TestServer::unused_addr();
    let _srv = build_test_server(ntex_server::NoConfig, async move |builder| {
        builder
            .configure(async move |cfg| {
                cfg.bind("addr", addr)?;
                Ok(())
            })
            .await
            .unwrap()
    });
    assert_eq!(read_tag(addr).map(|v| v.is_empty()).ok(), Some(true));
}

#[ntex::test]
async fn test_configure_error() {
    let res = build()
        .configure(async |_| Err(io::Error::other("config failed")))
        .await;
    assert_eq!(res.err().unwrap().to_string(), "config failed");

    let addr = TestServer::unused_addr();
    let res = build()
        .configure(async move |cfg| {
            let _lst = net::TcpListener::bind(addr)?;
            // the address is in use
            cfg.bind("addr", addr)?;
            Ok(())
        })
        .await;
    assert!(res.is_err());
}

/// A failing worker configuration stops the server with `stop_on_panic`.
#[ntex::test]
async fn test_on_worker_start_error() {
    let addr = TestServer::unused_addr();
    let srv = build_test_server(ntex_server::NoConfig, async move |builder| {
        builder
            .stop_on_panic()
            .configure(async move |cfg| {
                cfg.bind("addr", addr)?
                    .on_worker_start(async |_| Err(io::Error::other("start failed")));
                Ok(())
            })
            .await
            .unwrap()
    });
    ntex::time::timeout(Millis(5000), srv.server())
        .await
        .expect("server is not stopped")
        .unwrap();
}

/// A failing application state stops the server with `stop_on_panic`.
#[ntex::test]
async fn test_app_config_error() {
    let (lst, _) = local_listener();
    let srv = build_test_server(
        async || Err::<(), _>(io::Error::other("state failed")),
        async move |builder| {
            builder
                .stop_on_panic()
                .listen("test", lst, SharedCfg::default(), async |_: &()| {
                    fn_service(echo)
                })
                .unwrap()
        },
    );
    ntex::time::timeout(Millis(5000), srv.server())
        .await
        .expect("server is not stopped")
        .unwrap();
}

#[cfg(unix)]
#[ntex::test]
async fn test_uds() {
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = std::env::temp_dir();
    let path1 = dir.join(format!("ntex-server-{}-1.sock", std::process::id()));
    let path2 = dir.join(format!("ntex-server-{}-2.sock", std::process::id()));
    // stale socket file is removed
    std::fs::write(&path1, b"").unwrap();
    let _ = std::fs::remove_file(&path2);
    let lst = UnixListener::bind(&path2).unwrap();

    let p1 = path1.clone();
    let srv = build_test_server(ntex_server::NoConfig, async move |builder| {
        builder
            .bind_uds("uds1", &p1, SharedCfg::default(), async |_: &()| {
                fn_service(async |io| write_tag(io, b"uds1").await)
            })
            .unwrap()
            .listen_uds("uds2", lst, SharedCfg::default(), async |_: &()| {
                fn_service(async |io| write_tag(io, b"uds2").await)
            })
            .unwrap()
    });

    for (path, tag) in [(&path1, &b"uds1"[..]), (&path2, &b"uds2"[..])] {
        let mut conn = UnixStream::connect(path).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut buf = Vec::new();
        conn.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, tag);
    }

    // socket files are removed on stop
    srv.server().stop(true).await;
    wait_for(|| !path1.exists() && !path2.exists()).await;
}

#[cfg(unix)]
#[ntex::test]
async fn test_bind_uds_error() {
    let dir = std::env::temp_dir().join(format!("ntex-server-{}-dir", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    // the path is a non-empty directory
    let res = build().bind_uds("uds", &dir, SharedCfg::default(), async |_: &()| {
        fn_service(echo)
    });
    assert!(res.is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[ntex::test]
async fn test_pause_resume_status() {
    let (lst, addr) = local_listener();
    let srv = build_test_server(ntex_server::NoConfig, async move |builder| {
        builder
            .listen("test", lst, SharedCfg::default(), async |_: &()| {
                fn_service(echo)
            })
            .unwrap()
    })
    .set_addr(addr);
    let server = srv.server();
    ntex::time::timeout(Millis(3000), async {
        while server.status() != ServerStatus::Ready {
            server.status_changed().await;
        }
    })
    .await
    .unwrap();

    let io = srv.connect().await.unwrap();
    check_echo(&io).await;

    let changed = server.status_changed();
    server.pause().await;
    assert_eq!(server.status(), ServerStatus::Paused);
    assert_eq!(changed.await, ServerStatus::Paused);

    // the connection waits in the listen backlog
    let io2 = srv.connect().await.unwrap();
    io2.send(Bytes::from_static(b"hello"), &BytesCodec)
        .await
        .unwrap();
    assert!(
        ntex::time::timeout(Millis(200), io2.recv(&BytesCodec))
            .await
            .is_err()
    );
    // existing connections are active
    check_echo(&io).await;

    server.resume().await;
    assert_eq!(server.status(), ServerStatus::Ready);
    let msg = io2.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(msg, Bytes::from_static(b"hello"));

    server.stop(false).await;
    assert_eq!(server.status(), ServerStatus::Stopped);
    assert_eq!(server.status_changed().await, ServerStatus::Stopped);
    assert!(net::TcpStream::connect(addr).is_err());
}

struct ReadySrv {
    ready: Arc<AtomicUsize>,
    shutdown: Arc<AtomicUsize>,
}

impl Service<(), Io> for ReadySrv {
    type Res = ();
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        if self.ready.fetch_add(1, Relaxed) == 0 {
            Err(())
        } else {
            Ok(())
        }
    }

    async fn call(&self, io: Io, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        echo(io).await
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        self.shutdown.fetch_add(1, Relaxed);
    }
}

/// A failed readiness check re-creates the worker services.
#[ntex::test]
async fn test_ready_failure() {
    let created = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicUsize::new(0));
    let (c, r, s) = (created.clone(), ready.clone(), shutdown.clone());

    let srv = test_server(async move |_| {
        c.fetch_add(1, Relaxed);
        ReadySrv {
            ready: r.clone(),
            shutdown: s.clone(),
        }
    });

    let io = srv.connect().await.unwrap();
    check_echo(&io).await;
    assert_eq!(created.load(Relaxed), 2);
    wait_for(|| shutdown.load(Relaxed) == 1).await;

    srv.server().stop(true).await;
    assert_eq!(shutdown.load(Relaxed), 2);
}

/// Graceful stop waits for the worker service shutdown.
#[ntex::test]
async fn test_graceful_stop() {
    let (lst, addr) = local_listener();
    let srv = build_test_server(ntex_server::NoConfig, async move |builder| {
        builder
            .graceful_shutdown()
            .graceful_shutdown_timeout(Millis(2000))
            .listen("test", lst, SharedCfg::default(), async |_: &()| {
                fn_service(echo)
            })
            .unwrap()
    })
    .set_addr(addr);

    let io = srv.connect().await.unwrap();
    check_echo(&io).await;
    srv.server().stop(true).await;
    assert!(net::TcpStream::connect(addr).is_err());
}

#[ntex::test]
async fn test_server_state_bind() {
    let addr = TestServer::unused_addr();
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();
    let _srv = build_test_server(
        async move || Ok(St(num2.clone())),
        async move |builder| {
            builder
                .bind("test", addr, SharedCfg::default(), async |_: &St| {
                    async |st: &St, io: Io| {
                        st.0.fetch_add(1, Relaxed);
                        write_tag(io, b"state").await
                    }
                })
                .unwrap()
        },
    );
    assert_eq!(read_tag(addr).unwrap(), b"state");
    assert_eq!(num.load(Relaxed), 1);
}

/// STATEMENT: The server builder rejects a zero worker count when it is configured.
///
/// HOW:
/// - Call `ServerBuilder::workers(0)` before binding any listener.
/// - Expect the worker-count panic rather than a later socket or startup failure.
#[ntex::test]
#[should_panic(expected = "at least one worker")]
async fn test_builder_zero_workers() {
    drop(build().workers(0));
}

#[ntex::test]
#[should_panic(expected = "at least one bound socket")]
async fn test_run_without_sockets() {
    drop(build().run());
}

#[ntex::test]
async fn test_builder_debug() {
    let builder = build_with_config(ntex_server::NoConfig)
        .name("dbg")
        .workers(2)
        .affinity(Affinity::Disabled)
        .stop_runtime()
        .disable_signals();
    let s = format!("{builder:?}");
    assert!(s.contains("\"dbg\"") && s.contains("dbg:accept"), "{s}");

    let mut accept = AcceptLoop::default();
    accept.name("acc");
    accept.testing();
    let s = format!("{accept:?}");
    assert!(s.contains("acc:accept"), "{s}");
    // commands are queued without a running loop
    accept.notify().send(AcceptorCommand::Timer);
    assert!(format!("{:?}", AcceptorCommand::Pause).contains("Pause"));
}

#[test]
fn test_bind_addr() {
    let lst = bind_addr("127.0.0.1:0", 16).unwrap();
    assert_eq!(lst.len(), 1);

    // the address is in use
    let addr = lst[0].local_addr().unwrap();
    assert!(bind_addr(addr, 16).is_err());
    assert!(create_tcp_listener(addr, 16).is_err());
    assert!(bind_addr("not-an-address", 16).is_err());

    // IPv6 may be unavailable
    if let Ok(lst) = create_tcp_listener("[::1]:0".parse().unwrap(), 16) {
        assert!(lst.local_addr().unwrap().is_ipv6());
    }
}

/// A client can talk to a server with a raw socket.
#[ntex::test]
async fn test_raw_tcp() {
    let srv = test_server(async |_| fn_service(echo));
    let mut conn = net::TcpStream::connect(srv.addr()).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    conn.write_all(b"test").unwrap();
    let mut buf = [0u8; 4];
    conn.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"test");
}

/// Waits until the server has started its workers.
#[cfg(target_os = "linux")]
async fn wait_ready(srv: &ntex_server::Server<ntex_server::net::Connection>) {
    ntex::time::timeout(Millis(3000), async {
        while srv.status() != ServerStatus::Ready {
            srv.status_changed().await;
        }
    })
    .await
    .unwrap();
}

/// Returns the sorted names of the accept threads of server `name`.
#[cfg(target_os = "linux")]
fn accept_threads(name: &str) -> Vec<String> {
    let prefix = format!("{name}:accept");
    let mut names: Vec<String> = std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|e| std::fs::read_to_string(e.ok()?.path().join("comm")).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with(&prefix))
        .collect();
    names.sort();
    names
}

/// Returns the number of IPv4 TCP sockets listening on `port`.
#[cfg(target_os = "linux")]
fn listen_sockets(port: u16) -> usize {
    let port = format!(":{port:04X}");
    std::fs::read_to_string("/proc/net/tcp")
        .unwrap()
        .lines()
        .skip(1)
        .filter(|l| {
            let mut fields = l.split_whitespace().skip(1);
            let local = fields.next().unwrap_or_default();
            // state 0A is LISTEN
            local.ends_with(&port) && fields.nth(1) == Some("0A")
        })
        .count()
}

/// STATEMENT: With several acceptors, a TCP listener passed to `listen` is
/// replicated with `SO_REUSEPORT`, every accept thread serves connections,
/// and stopping the server closes all copies before the stop completes.
///
/// HOW:
/// - Start a server with `acceptors(4)` on a pre-bound listener.
/// - Check that four `rpa:accept*` threads run and four sockets listen on
///   the port.
/// - Open 32 connections, which the kernel spreads over the four sockets,
///   and check that each one echoes within a timeout, so no copy is left
///   without a running accept thread.
/// - Check that the listener metrics count 32 accepted connections.
/// - Stop the server and check that no socket listens on the port.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_acceptors_replicate_listener() {
    let (lst, addr) = local_listener();
    let srv = build()
        .name("rpa")
        .workers(2)
        .acceptors(4)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    assert_eq!(
        accept_threads("rpa"),
        ["rpa:accept", "rpa:accept1", "rpa:accept2", "rpa:accept3"]
    );
    assert_eq!(listen_sockets(addr.port()), 4);

    let mut conns = Vec::new();
    for _ in 0..32 {
        let io = ntex_net::tcp_connect(addr, SharedCfg::default())
            .await
            .unwrap();
        ntex::time::timeout(Millis(5000), check_echo(&io))
            .await
            .unwrap();
        conns.push(io);
    }
    assert_eq!(srv.metrics().listeners[0].accepted, 32);

    srv.stop(false).await;
    assert_eq!(listen_sockets(addr.port()), 0);
}

/// STATEMENT: Pause and resume reach every accept thread.
///
/// HOW:
/// - Start a server with `acceptors(3)`, wait until it is ready and pause it.
/// - Open 12 connections that send a message, and check that none is
///   accepted while paused.
/// - Resume the server and check that every connection echoes and the
///   listener metrics count 12 accepted connections.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_acceptors_pause_resume() {
    let (lst, addr) = local_listener();
    let srv = build()
        .name("prs")
        .workers(1)
        .acceptors(3)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    assert_eq!(accept_threads("prs").len(), 3);
    wait_ready(&srv).await;

    srv.pause().await;
    // the accept threads handle the command asynchronously
    sleep(Millis(100)).await;

    let mut conns = Vec::new();
    for _ in 0..12 {
        let io = ntex_net::tcp_connect(addr, SharedCfg::default())
            .await
            .unwrap();
        io.send(Bytes::from_static(b"hello"), &BytesCodec)
            .await
            .unwrap();
        conns.push(io);
    }
    sleep(Millis(200)).await;
    assert_eq!(srv.metrics().listeners[0].accepted, 0);

    srv.resume().await;
    for io in &conns {
        let msg = ntex::time::timeout(Millis(5000), io.recv(&BytesCodec))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(msg, Bytes::from_static(b"hello"));
    }
    assert_eq!(srv.metrics().listeners[0].accepted, 12);
    srv.stop(false).await;
}

/// STATEMENT: Unix domain listeners are not replicated, so a server with
/// only Unix domain listeners runs a single accept thread.
///
/// HOW:
/// - Start a server with `acceptors(3)` and a single `bind_uds` listener.
/// - Check that only the `uds3:accept` thread runs and, once the server is
///   ready, that the socket echoes.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_acceptors_uds_single_thread() {
    let path = std::env::temp_dir().join(format!("ntex-acceptors-{}.sock", std::process::id()));
    let srv = build()
        .name("uds3")
        .workers(1)
        .acceptors(3)
        .disable_signals()
        .bind_uds("test", &path, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    assert_eq!(accept_threads("uds3"), ["uds3:accept"]);
    wait_ready(&srv).await;

    let io = ntex_net::unix_connect(&path, SharedCfg::default())
        .await
        .unwrap();
    ntex::time::timeout(Millis(5000), check_echo(&io))
        .await
        .unwrap();
    srv.stop(false).await;
}

/// Returns whether workers accept connections themselves with
/// `LoadBalancing::Reuseport` on the current runtime.
#[cfg(target_os = "linux")]
fn reuseport_active() -> bool {
    ntex_net::is_tcp_listener_supported()
}

/// Opens a connection that sends a message without waiting for the reply.
#[cfg(target_os = "linux")]
async fn send_hello(addr: net::SocketAddr) -> Io {
    let io = ntex_net::tcp_connect(addr, SharedCfg::default())
        .await
        .unwrap();
    io.send(Bytes::from_static(b"hello"), &BytesCodec)
        .await
        .unwrap();
    io
}

/// Waits for the reply to `send_hello`.
#[cfg(target_os = "linux")]
async fn recv_hello(io: &Io) {
    let msg = ntex::time::timeout(Millis(5000), io.recv(&BytesCodec))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(msg, Bytes::from_static(b"hello"));
}

/// STATEMENT: With `LoadBalancing::Reuseport`, every worker accepts
/// connections on its own `SO_REUSEPORT` socket without an accept thread,
/// and stopping the server closes all sockets. On a runtime without
/// worker-side accept, the accept thread serves the listener.
///
/// HOW:
/// - Start three workers without CPU affinity, so the kernel spreads
///   connections by hash, on a pre-bound listener.
/// - With worker-side accept, check that no accept thread runs and three
///   sockets listen on the port; otherwise check for one accept thread and
///   one socket.
/// - Open 30 connections and check that each echoes, that the listener and
///   worker metrics count 30 connections, and, with worker-side accept,
///   that every worker took some.
/// - Stop the server and check that all sockets close and connecting fails.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_reuseport_workers_accept() {
    let (lst, addr) = local_listener();
    let srv = build()
        .name("rpw")
        .workers(3)
        .affinity(Affinity::Disabled)
        .load_balancing(LoadBalancing::Reuseport)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    wait_ready(&srv).await;
    let active = reuseport_active();
    if active {
        assert!(accept_threads("rpw").is_empty());
        assert_eq!(listen_sockets(addr.port()), 3);
    } else {
        assert_eq!(accept_threads("rpw"), ["rpw:accept"]);
        assert_eq!(listen_sockets(addr.port()), 1);
    }

    let mut conns = Vec::new();
    for _ in 0..30 {
        let io = ntex_net::tcp_connect(addr, SharedCfg::default())
            .await
            .unwrap();
        ntex::time::timeout(Millis(5000), check_echo(&io))
            .await
            .unwrap();
        conns.push(io);
    }
    let m = srv.metrics();
    assert_eq!(m.listeners[0].accepted, 30);
    assert_eq!(m.workers.iter().map(|w| w.items).sum::<u64>(), 30);
    if active {
        assert!(m.workers.iter().all(|w| w.items > 0), "{:?}", m.workers);
    }

    srv.stop(false).await;
    // a non-graceful stop does not wait for the workers to close their sockets
    wait_for(|| listen_sockets(addr.port()) == 0).await;
    assert!(net::TcpStream::connect(addr).is_err());
}

/// STATEMENT: Pause and resume reach the workers' own listeners.
///
/// HOW:
/// - Start two workers with `LoadBalancing::Reuseport`, wait until the
///   server is ready and pause it.
/// - Open 12 connections that send a message, and check that none is
///   accepted while paused and that the pause is counted.
/// - Resume the server and check that every connection echoes and the
///   listener metrics count 12 accepted connections.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_reuseport_pause_resume() {
    let (lst, addr) = local_listener();
    let srv = build()
        .workers(2)
        .load_balancing(LoadBalancing::Reuseport)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    wait_ready(&srv).await;

    srv.pause().await;
    // the listeners handle the command asynchronously
    sleep(Millis(100)).await;

    let mut conns = Vec::new();
    for _ in 0..12 {
        conns.push(send_hello(addr).await);
    }
    sleep(Millis(200)).await;
    let m = srv.metrics();
    assert_eq!(m.listeners[0].accepted, 0);
    assert!(m.accept_paused_duration > Duration::ZERO, "{m:?}");

    srv.resume().await;
    for io in &conns {
        recv_hello(io).await;
    }
    assert_eq!(srv.metrics().listeners[0].accepted, 12);
    srv.stop(false).await;
}

/// STATEMENT: A worker at its connection limit stops accepting, so new
/// connections wait in the listen backlog until a connection closes.
///
/// HOW:
/// - Start one worker with `LoadBalancing::Reuseport` and
///   `max_connections(1)`.
/// - Open a connection and check that it echoes.
/// - Open a second connection that sends a message, and check that it gets
///   no reply and is not accepted within 300 ms.
/// - Close the first connection and check that the second one echoes and
///   two connections are accepted.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_reuseport_max_connections() {
    let (lst, addr) = local_listener();
    let srv = build()
        .workers(1)
        .max_connections(1)
        .load_balancing(LoadBalancing::Reuseport)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    wait_ready(&srv).await;

    let first = send_hello(addr).await;
    recv_hello(&first).await;

    let second = send_hello(addr).await;
    let res = ntex::time::timeout(Millis(300), second.recv(&BytesCodec)).await;
    assert!(res.is_err(), "{res:?}");
    assert_eq!(srv.metrics().listeners[0].accepted, 1);

    first.close();
    drop(first);
    recv_hello(&second).await;
    assert_eq!(srv.metrics().listeners[0].accepted, 2);
    srv.stop(false).await;
}

/// STATEMENT: A worker keeps accepting on its own listener after its
/// service is re-created.
///
/// HOW:
/// - Start one worker with `LoadBalancing::Reuseport` whose service fails
///   its first readiness check.
/// - Wait until the failed service is shut down, then check that a new
///   connection echoes and is counted by the listener metrics.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_reuseport_service_recreated() {
    let (lst, addr) = local_listener();
    let ready = Arc::new(AtomicUsize::new(0));
    let shutdown = Arc::new(AtomicUsize::new(0));
    let (r, s) = (ready.clone(), shutdown.clone());
    let srv = build()
        .workers(1)
        .load_balancing(LoadBalancing::Reuseport)
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async move |_: &()| {
            ReadySrv {
                ready: r.clone(),
                shutdown: s.clone(),
            }
        })
        .unwrap()
        .run();
    wait_for(|| shutdown.load(Relaxed) == 1).await;

    let io = send_hello(addr).await;
    recv_hello(&io).await;
    assert_eq!(srv.metrics().listeners[0].accepted, 1);
    srv.stop(false).await;
}

/// STATEMENT: Unix domain listeners are served by the accept thread while
/// TCP listeners of the same server use the workers' own sockets.
///
/// HOW:
/// - Start two workers with `LoadBalancing::Reuseport`, a TCP and a Unix
///   domain listener.
/// - Check that one accept thread runs and, with worker-side accept, that
///   two sockets listen on the TCP port.
/// - Check that both listeners echo and each counts one connection.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_reuseport_with_uds() {
    let (lst, addr) = local_listener();
    let path = std::env::temp_dir().join(format!("ntex-reuseport-{}.sock", std::process::id()));
    let srv = build()
        .name("rpu")
        .workers(2)
        .load_balancing(LoadBalancing::Reuseport)
        .disable_signals()
        .listen("tcp", lst, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .bind_uds("uds", &path, SharedCfg::default(), async |_: &()| {
            fn_service(echo)
        })
        .unwrap()
        .run();
    wait_ready(&srv).await;
    assert_eq!(accept_threads("rpu"), ["rpu:accept"]);
    let sockets = if reuseport_active() { 2 } else { 1 };
    assert_eq!(listen_sockets(addr.port()), sockets);

    let io = send_hello(addr).await;
    recv_hello(&io).await;
    let io = ntex_net::unix_connect(&path, SharedCfg::default())
        .await
        .unwrap();
    ntex::time::timeout(Millis(5000), check_echo(&io))
        .await
        .unwrap();
    let m = srv.metrics();
    assert_eq!(m.listeners[0].accepted, 1);
    assert_eq!(m.listeners[1].accepted, 1);
    srv.stop(false).await;
}

/// Echo service whose shutdown waits until its own connections close.
#[cfg(target_os = "linux")]
struct DrainSrv {
    active: Arc<AtomicUsize>,
    drained: Arc<AtomicUsize>,
}

#[cfg(target_os = "linux")]
impl Service<(), Io> for DrainSrv {
    type Res = ();
    type Error = ();

    async fn call(&self, io: Io, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
        self.active.fetch_add(1, Relaxed);
        let res = echo(io).await;
        self.active.fetch_sub(1, Relaxed);
        res
    }

    async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
        while self.active.load(Relaxed) > 0 {
            sleep(Millis(10)).await;
        }
        self.drained.fetch_add(1, Relaxed);
    }
}

/// STATEMENT: A graceful stop with `LoadBalancing::Reuseport` closes the
/// workers' sockets right away, while open connections are served until
/// the worker services shut down.
///
/// HOW:
/// - Start two workers with `LoadBalancing::Reuseport` whose services wait
///   for their connections to close on shutdown.
/// - Open a connection and check that it echoes.
/// - Start a graceful stop, wait until no socket listens on the port, and
///   check that new connections are refused, the open connection still
///   echoes, and the stop has not completed.
/// - Close the connection and check that the stop completes after both
///   services drained.
#[cfg(target_os = "linux")]
#[ntex::test]
async fn test_reuseport_graceful_stop() {
    let (lst, addr) = local_listener();
    let drained = Arc::new(AtomicUsize::new(0));
    let d = drained.clone();
    let srv = build()
        .workers(2)
        .load_balancing(LoadBalancing::Reuseport)
        .graceful_shutdown_timeout(Millis(5000))
        .disable_signals()
        .listen("test", lst, SharedCfg::default(), async move |_: &()| {
            DrainSrv {
                active: Arc::new(AtomicUsize::new(0)),
                drained: d.clone(),
            }
        })
        .unwrap()
        .run();
    wait_ready(&srv).await;

    let io = send_hello(addr).await;
    recv_hello(&io).await;

    let stopped = Arc::new(AtomicUsize::new(0));
    let (s, st) = (srv.clone(), stopped.clone());
    ntex::rt::spawn(async move {
        s.stop(true).await;
        st.store(1, Relaxed);
    });
    wait_for(|| listen_sockets(addr.port()) == 0).await;
    assert!(net::TcpStream::connect(addr).is_err());

    check_echo(&io).await;
    sleep(Millis(100)).await;
    assert_eq!(stopped.load(Relaxed), 0);
    assert_eq!(drained.load(Relaxed), 1);

    io.close();
    drop(io);
    wait_for(|| stopped.load(Relaxed) == 1).await;
    assert_eq!(drained.load(Relaxed), 2);
}
