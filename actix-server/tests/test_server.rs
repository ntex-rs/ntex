use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::{net, thread, time};

use actix_codec::{BytesCodec, Framed};
use actix_rt::net::TcpStream;
use actix_server::Server;
use actix_service::service_fn;
use bytes::Bytes;
use futures::future::{lazy, ok};
use futures::SinkExt;
use net2::TcpBuilder;

fn unused_addr() -> net::SocketAddr {
    let addr: net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let socket = TcpBuilder::new_v4().unwrap();
    socket.bind(&addr).unwrap();
    socket.reuse_address(true).unwrap();
    let tcp = socket.to_tcp_listener().unwrap();
    tcp.local_addr().unwrap()
}

#[test]
fn test_bind() {
    let addr = unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = actix_rt::System::new("test");
        let srv = Server::build()
            .workers(1)
            .disable_signals()
            .bind("test", addr, move || service_fn(|_| ok::<_, ()>(())))
            .unwrap()
            .start();
        let _ = tx.send((srv, actix_rt::System::current()));
        let _ = sys.run();
    });
    let (_, sys) = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
    let _ = sys.stop();
    let _ = h.join();
}

#[test]
fn test_listen() {
    let addr = unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = actix_rt::System::new("test");
        let lst = net::TcpListener::bind(addr).unwrap();
        Server::build()
            .disable_signals()
            .workers(1)
            .listen("test", lst, move || service_fn(|_| ok::<_, ()>(())))
            .unwrap()
            .start();
        let _ = tx.send(actix_rt::System::current());
        let _ = sys.run();
    });
    let sys = rx.recv().unwrap();

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
    let _ = sys.stop();
    let _ = h.join();
}

#[test]
#[cfg(unix)]
fn test_start() {
    let addr = unused_addr();
    let (tx, rx) = mpsc::channel();

    let _ = thread::spawn(move || {
        let sys = actix_rt::System::new("test");
        let srv: Server = Server::build()
            .backlog(100)
            .workers(1)
            .disable_signals()
            .bind("test", addr, move || {
                service_fn(|io: TcpStream| {
                    async move {
                        let mut f = Framed::new(io, BytesCodec);
                        f.send(Bytes::from_static(b"test")).await.unwrap();
                        Ok::<_, ()>(())
                    }
                })
            })
            .unwrap()
            .start();

        let _ = tx.send((srv, actix_rt::System::current()));
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
    let _ = sys.stop();
    // let _ = h.join();
}

#[test]
fn test_configure() {
    let addr1 = unused_addr();
    let addr2 = unused_addr();
    let addr3 = unused_addr();
    let (tx, rx) = mpsc::channel();
    let num = Arc::new(AtomicUsize::new(0));
    let num2 = num.clone();

    let h = thread::spawn(move || {
        let num = num2.clone();
        let sys = actix_rt::System::new("test");
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
                    .apply(move |rt| {
                        let num = num.clone();
                        rt.service("addr1", service_fn(|_| ok::<_, ()>(())));
                        rt.service("addr3", service_fn(|_| ok::<_, ()>(())));
                        rt.on_start(lazy(move |_| {
                            let _ = num.fetch_add(1, Relaxed);
                        }))
                    })
            })
            .unwrap()
            .workers(1)
            .start();
        let _ = tx.send((srv, actix_rt::System::current()));
        let _ = sys.run();
    });
    let (_, sys) = rx.recv().unwrap();
    thread::sleep(time::Duration::from_millis(500));

    assert!(net::TcpStream::connect(addr1).is_ok());
    assert!(net::TcpStream::connect(addr2).is_ok());
    assert!(net::TcpStream::connect(addr3).is_ok());
    assert_eq!(num.load(Relaxed), 1);
    let _ = sys.stop();
    let _ = h.join();
}
