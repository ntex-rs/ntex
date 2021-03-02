use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::{io, io::Read, net, thread, time};

use bytes::Bytes;
use futures::future::{lazy, ok, FutureExt};
use futures::SinkExt;

use ntex::codec::{BytesCodec, Framed};
use ntex::rt::net::TcpStream;
use ntex::server::{Server, TestServer};
use ntex::service::fn_service;

#[test]
fn test_bind() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");
        let srv = sys.exec(|| {
            Server::build()
                .workers(1)
                .disable_signals()
                .bind("test", addr, move || fn_service(|_| ok::<_, ()>(())))
                .unwrap()
                .start()
        });
        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (_, sys) = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(300));
    assert!(net::TcpStream::connect(addr).is_ok());
    sys.stop();
    let _ = h.join();
}

#[test]
fn test_listen() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");
        let lst = net::TcpListener::bind(addr).unwrap();
        sys.exec(|| {
            Server::build()
                .disable_signals()
                .workers(1)
                .listen("test", lst, move || fn_service(|_| ok::<_, ()>(())))
                .unwrap()
                .start()
        });
        let _ = tx.send(ntex::rt::System::current());
        let _ = sys.run();
    });
    let sys = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
    sys.stop();
    let _ = h.join();
}

#[test]
#[cfg(unix)]
fn test_start() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");
        let srv = sys.exec(|| {
            Server::build()
                .backlog(100)
                .disable_signals()
                .bind("test", addr, move || {
                    fn_service(|io: TcpStream| async move {
                        let mut f = Framed::new(io, BytesCodec);
                        f.send(Bytes::from_static(b"test")).await.unwrap();
                        Ok::<_, ()>(())
                    })
                })
                .unwrap()
                .start()
        });

        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (srv, sys) = rx.recv().unwrap();

    let mut buf = [1u8; 4];
    let mut conn = net::TcpStream::connect(addr).unwrap();
    let _ = conn.read_exact(&mut buf);
    assert_eq!(buf, b"test"[..]);

    // pause
    let _ = srv.pause();
    thread::sleep(time::Duration::from_millis(200));
    let mut conn = net::TcpStream::connect(addr).unwrap();
    conn.set_read_timeout(Some(time::Duration::from_millis(100)))
        .unwrap();
    let res = conn.read_exact(&mut buf);
    assert!(res.is_err());

    // resume
    let _ = srv.resume();
    thread::sleep(time::Duration::from_millis(100));
    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());

    let mut buf = [0u8; 4];
    let mut conn = net::TcpStream::connect(addr).unwrap();
    let _ = conn.read_exact(&mut buf);
    assert_eq!(buf, b"test"[..]);

    // stop
    let _ = srv.stop(false);
    thread::sleep(time::Duration::from_millis(100));
    assert!(net::TcpStream::connect(addr).is_err());

    thread::sleep(time::Duration::from_millis(100));
    sys.stop();
    let _ = h.join();
}

#[test]
fn test_configure() {
    let addr1 = TestServer::unused_addr();
    let addr2 = TestServer::unused_addr();
    let addr3 = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let h = thread::spawn(move || {
        let num = num2.clone();
        let mut sys = ntex::rt::System::new("test");
        let srv = sys.exec(|| {
            Server::build()
                .disable_signals()
                .configure(move |cfg| {
                    let num = num.clone();
                    let lst = net::TcpListener::bind(addr3).unwrap();
                    cfg.bind("addr1", addr1)
                        .unwrap()
                        .bind("addr2", addr2)
                        .unwrap()
                        .listen("addr3", lst)
                        .apply(move |rt| {
                            let num = num.clone();
                            rt.service("addr1", fn_service(|_| ok::<_, ()>(())));
                            rt.service("addr3", fn_service(|_| ok::<_, ()>(())));
                            rt.on_start(lazy(move |_| {
                                let _ = num.fetch_add(1, Relaxed);
                            }))
                        })
                })
                .unwrap()
                .workers(1)
                .start()
        });
        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (_, sys) = rx.recv().unwrap();
    thread::sleep(time::Duration::from_millis(500));

    assert!(net::TcpStream::connect(addr1).is_ok());
    assert!(net::TcpStream::connect(addr2).is_ok());
    assert!(net::TcpStream::connect(addr3).is_ok());
    assert_eq!(num.load(Relaxed), 1);
    sys.stop();
    let _ = h.join();
}

#[test]
fn test_configure_async() {
    let addr1 = TestServer::unused_addr();
    let addr2 = TestServer::unused_addr();
    let addr3 = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let h = thread::spawn(move || {
        let num = num2.clone();
        let mut sys = ntex::rt::System::new("test");
        let srv = sys.exec(|| {
            Server::build()
                .disable_signals()
                .configure(move |cfg| {
                    let num = num.clone();
                    let lst = net::TcpListener::bind(addr3).unwrap();
                    cfg.bind("addr1", addr1)
                        .unwrap()
                        .bind("addr2", addr2)
                        .unwrap()
                        .listen("addr3", lst)
                        .apply_async(move |rt| {
                            let num = num.clone();
                            async move {
                                rt.service("addr1", fn_service(|_| ok::<_, ()>(())));
                                rt.service("addr3", fn_service(|_| ok::<_, ()>(())));
                                rt.on_start(lazy(move |_| {
                                    let _ = num.fetch_add(1, Relaxed);
                                }));
                                Ok::<_, io::Error>(())
                            }
                        })
                })
                .unwrap()
                .workers(1)
                .start()
        });
        let _ = tx.send((srv, ntex::rt::System::current()));
        let _ = sys.run();
    });
    let (_, sys) = rx.recv().unwrap();
    thread::sleep(time::Duration::from_millis(500));

    assert!(net::TcpStream::connect(addr1).is_ok());
    assert!(net::TcpStream::connect(addr2).is_ok());
    assert!(net::TcpStream::connect(addr3).is_ok());
    assert_eq!(num.load(Relaxed), 1);
    sys.stop();
    let _ = h.join();
}

#[test]
#[allow(unreachable_code)]
fn test_panic_in_worker() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter2 = counter.clone();

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let mut sys = ntex::rt::System::new("test");
        let counter = counter2.clone();
        let srv = sys.exec(move || {
            let counter = counter.clone();
            Server::build()
                .workers(1)
                .disable_signals()
                .bind("test", addr, move || {
                    let counter = counter.clone();
                    fn_service(move |_| {
                        counter.fetch_add(1, Relaxed);
                        panic!();
                        ok::<_, ()>(())
                    })
                })
                .unwrap()
                .start()
        });
        let _ = tx.send((srv.clone(), ntex::rt::System::current()));
        sys.exec(move || ntex_rt::spawn(srv.map(|_| ())));
        let _ = sys.run();
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
    assert_eq!(counter.load(Relaxed), 2);

    sys.stop();
    let _ = h.join();
}
