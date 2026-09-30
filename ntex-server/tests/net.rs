//! Network server integration tests.
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::{net, time::Duration};

use ntex::codec::BytesCodec;
use ntex::time::{Millis, sleep};
use ntex::util::Bytes;
use ntex_io::Io;
use ntex_server::net::{
    AcceptLoop, AcceptorCommand, ServerBuilder, ServerStatus, TestServer, TestServerBuilder,
    bind_addr, build, build_test_server, build_with_config, create_tcp_listener, test_server,
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
    let srv = test_server(async || fn_service(echo));
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

#[ntex::test]
async fn test_server_builder() {
    let (tx, rx) = mpsc::channel();
    let builder = TestServerBuilder::new(async move || {
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
        async || {
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
    let (tx, rx) = mpsc::channel();
    let srv = build_test_server(ntex_server::NoConfig, async move |builder| {
        builder
            .status_handler(move |st| {
                let _ = tx.send(st);
            })
            .listen("test", lst, SharedCfg::default(), async |_: &()| {
                fn_service(echo)
            })
            .unwrap()
    })
    .set_addr(addr);
    let recv = || rx.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(recv(), ServerStatus::Ready);

    let io = srv.connect().await.unwrap();
    check_echo(&io).await;

    let server = srv.server();
    server.pause().await;
    assert_eq!(recv(), ServerStatus::NotReady);

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
    assert_eq!(recv(), ServerStatus::Ready);
    let msg = io2.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(msg, Bytes::from_static(b"hello"));

    server.stop(false).await;
    assert_eq!(recv(), ServerStatus::NotReady);
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

    let srv = test_server(async move || {
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
        .enable_affinity()
        .stop_runtime()
        .disable_signals();
    let s = format!("{builder:?}");
    assert!(s.contains("\"dbg\"") && s.contains("dbg:accept"), "{s}");

    let mut accept = AcceptLoop::default();
    accept.name("acc");
    accept.testing();
    accept.set_status_handler(|_| ());
    let s = format!("{accept:?}");
    assert!(s.contains("acc:accept") && s.contains("status_handler: true"));
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
    let srv = test_server(async || fn_service(echo));
    let mut conn = net::TcpStream::connect(srv.addr()).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    conn.write_all(b"test").unwrap();
    let mut buf = [0u8; 4];
    conn.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"test");
}
