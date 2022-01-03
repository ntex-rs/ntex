use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::{io, io::Read, net, thread, time};

use futures::future::{ok, FutureExt};

use ntex::codec::BytesCodec;
use ntex::io::Io;
use ntex::server::{Server, TestServer};
use ntex::service::fn_service;
use ntex::util::{Bytes, Ready};

#[test]
fn test_bind() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test");
        sys.run(move || {
            let srv = Server::build()
                .workers(1)
                .disable_signals()
                .bind("test", addr, move |_| fn_service(|_| ok::<_, ()>(())))
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

#[test]
fn test_listen() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test");
        let lst = net::TcpListener::bind(addr).unwrap();
        sys.run(move || {
            Server::build()
                .disable_signals()
                .workers(1)
                .listen("test", lst, move |_| fn_service(|_| ok::<_, ()>(())))
                .unwrap()
                .run();
            let _ = tx.send(ntex::rt::System::current());
            Ok(())
        })
    });
    let sys = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
    sys.stop();
    let _ = h.join();
}

#[test]
#[cfg(unix)]
fn test_run() {
    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test");
        sys.run(move || {
            let srv = Server::build()
                .backlog(100)
                .disable_signals()
                .bind("test", addr, move |_| {
                    fn_service(|io: Io| async move {
                        io.send(Bytes::from_static(b"test"), &BytesCodec)
                            .await
                            .unwrap();
                        Ok::<_, ()>(())
                    })
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
        let sys = ntex::rt::System::new("test");
        sys.run(move || {
            let srv = Server::build()
                .disable_signals()
                .configure(move |cfg| {
                    let num = num.clone();
                    let lst = net::TcpListener::bind(addr3).unwrap();
                    cfg.bind("addr1", addr1)
                        .unwrap()
                        .bind("addr2", addr2)
                        .unwrap()
                        .listen("addr3", lst)
                        .on_worker_start(move |rt| {
                            let num = num.clone();
                            async move {
                                rt.service("addr1", fn_service(|_| ok::<_, ()>(())));
                                rt.service("addr3", fn_service(|_| ok::<_, ()>(())));
                                let _ = num.fetch_add(1, Relaxed);
                                Ok::<_, io::Error>(())
                            }
                        })
                        .unwrap();
                    Ok::<_, io::Error>(())
                })
                .unwrap()
                .on_worker_start(move |_| {
                    let _ = num2.fetch_add(1, Relaxed);
                    Ready::Ok::<_, io::Error>(())
                })
                .workers(1)
                .run();
            let _ = tx.send((srv, ntex::rt::System::current()));
            Ok(())
        })
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
#[allow(unreachable_code)]
fn test_panic_in_worker() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter2 = counter.clone();

    let addr = TestServer::unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = ntex::rt::System::new("test");
        let counter = counter2.clone();
        sys.run(move || {
            let counter = counter.clone();
            let srv = Server::build()
                .workers(1)
                .disable_signals()
                .bind("test", addr, move |_| {
                    let counter = counter.clone();
                    fn_service(move |_| {
                        counter.fetch_add(1, Relaxed);
                        panic!();
                        ok::<_, ()>(())
                    })
                })
                .unwrap()
                .run();
            let _ = tx.send((srv.clone(), ntex::rt::System::current()));
            ntex::rt::spawn(srv.map(|_| ()));
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
    assert_eq!(counter.load(Relaxed), 2);

    sys.stop();
    let _ = h.join();
}
