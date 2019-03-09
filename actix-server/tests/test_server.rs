use std::{net, thread, time};

use actix_server::Server;
use actix_service::fn_service;
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
            .bind("test", addr, || fn_service(|_| Ok::<_, ()>(())))
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
            .listen("test", lst, move || fn_service(|_| Ok::<_, ()>(())))
            .unwrap()
            .run()
    });

    thread::sleep(time::Duration::from_millis(500));
    assert!(net::TcpStream::connect(addr).is_ok());
}
