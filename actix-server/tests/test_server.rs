use std::io::Read;
use std::sync::mpsc;
use std::{net, thread, time};

use actix_codec::{BytesCodec, Framed};
use actix_server::{Io, Server, ServerConfig};
use actix_service::{factory_fn_cfg, service_fn, service_fn2};
use bytes::Bytes;
use futures::{future::ok, SinkExt};
use net2::TcpBuilder;
use tokio_net::tcp::TcpStream;

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
            .bind("test", addr, move || {
                factory_fn_cfg(move |cfg: &ServerConfig| {
                    assert_eq!(cfg.local_addr(), addr);
                    ok::<_, ()>(service_fn2(|_| ok::<_, ()>(())))
                })
            })
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
fn test_bind_no_config() {
    let addr = unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = actix_rt::System::new("test");
        let srv = Server::build()
            .bind("test", addr, move || service_fn(|_| ok::<_, ()>(())))
            .unwrap()
            .start();
        let _ = tx.send((srv, actix_rt::System::current()));
        let _ = sys.run();
    });
    let (_, sys) = rx.recv().unwrap();
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
        let srv = Server::build()
            .listen("test", lst, move || {
                factory_fn_cfg(move |cfg: &ServerConfig| {
                    assert_eq!(cfg.local_addr(), addr);
                    ok::<_, ()>(service_fn2(|_| ok::<_, ()>(())))
                })
            })
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
#[cfg(unix)]
fn test_start() {
    let addr = unused_addr();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        let sys = actix_rt::System::new("test");
        let srv: Server = Server::build()
            .backlog(100)
            .bind("test", addr, move || {
                factory_fn_cfg(move |cfg: &ServerConfig| {
                    assert_eq!(cfg.local_addr(), addr);

                    let srv = service_fn2(|io: Io<TcpStream>| {
                        async {
                            let mut f = Framed::new(io.into_parts().0, BytesCodec);
                            f.send(Bytes::from_static(b"test")).await.unwrap();
                            Ok::<_, ()>(())
                        }
                    });

                    ok::<_, ()>(srv)
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
    let _ = h.join();
}
