use std::sync::mpsc;
use std::{net, thread, time};

use actix_server::{Server, ServerConfig};
use actix_service::{fn_cfg_factory, fn_service, IntoService};
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

    thread::spawn(move || {
        Server::build()
            .bind("test", addr, move || {
                fn_cfg_factory(move |cfg: &ServerConfig| {
                    assert_eq!(cfg.local_addr(), addr);
                    Ok::<_, ()>((|_| Ok::<_, ()>(())).into_service())
                })
            })
            .unwrap()
            .run()
    });

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
}

#[test]
fn test_bind_no_config() {
    let addr = unused_addr();

    thread::spawn(move || {
        Server::build()
            .bind("test", addr, move || fn_service(|_| Ok::<_, ()>(())))
            .unwrap()
            .run()
    });

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
}

#[test]
fn test_listen() {
    let addr = unused_addr();

    thread::spawn(move || {
        let lst = net::TcpListener::bind(addr).unwrap();
        Server::build()
            .listen("test", lst, move || {
                fn_cfg_factory(move |cfg: &ServerConfig| {
                    assert_eq!(cfg.local_addr(), addr);
                    Ok::<_, ()>((|_| Ok::<_, ()>(())).into_service())
                })
            })
            .unwrap()
            .run()
    });

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
}

#[test]
#[cfg(unix)]
fn test_start() {
    let addr = unused_addr();
    let (tx, rx) = mpsc::channel();

    thread::spawn(move || {
        let sys = actix_rt::System::new("test");

        let srv = Server::build()
            .backlog(1)
            .bind("test", addr, move || {
                fn_cfg_factory(move |cfg: &ServerConfig| {
                    assert_eq!(cfg.local_addr(), addr);
                    Ok::<_, ()>((|_| Ok::<_, ()>(())).into_service())
                })
            })
            .unwrap()
            .start();

        let _ = tx.send((srv, actix_rt::System::current()));
        let _ = sys.run();
    });
    let (srv, sys) = rx.recv().unwrap();
    thread::sleep(time::Duration::from_millis(400));

    assert!(net::TcpStream::connect(addr).is_ok());

    // pause
    let _ = srv.pause();
    thread::sleep(time::Duration::from_millis(100));
    assert!(net::TcpStream::connect_timeout(&addr, time::Duration::from_millis(100)).is_ok());
    thread::sleep(time::Duration::from_millis(400));
    assert!(net::TcpStream::connect_timeout(&addr, time::Duration::from_millis(100)).is_err());

    // resume
    let _ = srv.resume();
    thread::sleep(time::Duration::from_millis(100));
    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());
    assert!(net::TcpStream::connect(addr).is_ok());

    // stop
    let _ = srv.stop(false);
    thread::sleep(time::Duration::from_millis(100));
    assert!(net::TcpStream::connect(addr).is_err());

    let _ = sys.stop();
}
