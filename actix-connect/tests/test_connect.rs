use actix_codec::{BytesCodec, Framed};
use actix_server_config::Io;
use actix_service::{fn_service, NewService, Service};
use actix_test_server::TestServer;
use bytes::Bytes;
use futures::{future::lazy, Future, Sink};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};

use actix_connect::{default_connector, Connect};

#[test]
fn test_string() {
    let mut srv = TestServer::with(|| {
        fn_service(|io: Io<tokio_tcp::TcpStream>| {
            Framed::new(io.into_parts().0, BytesCodec)
                .send(Bytes::from_static(b"test"))
                .then(|_| Ok::<_, ()>(()))
        })
    });

    let mut conn = srv
        .block_on(lazy(|| Ok::<_, ()>(default_connector())))
        .unwrap();
    let addr = format!("localhost:{}", srv.port());
    let con = srv.block_on(conn.call(addr.into())).unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[test]
fn test_static_str() {
    let mut srv = TestServer::with(|| {
        fn_service(|io: Io<tokio_tcp::TcpStream>| {
            Framed::new(io.into_parts().0, BytesCodec)
                .send(Bytes::from_static(b"test"))
                .then(|_| Ok::<_, ()>(()))
        })
    });

    let resolver = srv
        .block_on(lazy(
            || Ok::<_, ()>(actix_connect::start_default_resolver()),
        ))
        .unwrap();
    let mut conn = srv
        .block_on(lazy(|| Ok::<_, ()>(actix_connect::new_connector(resolver))))
        .unwrap();

    let con = srv
        .block_on(conn.call(Connect::with("10", srv.addr())))
        .unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[test]
fn test_new_service() {
    let mut srv = TestServer::with(|| {
        fn_service(|io: Io<tokio_tcp::TcpStream>| {
            Framed::new(io.into_parts().0, BytesCodec)
                .send(Bytes::from_static(b"test"))
                .then(|_| Ok::<_, ()>(()))
        })
    });

    let resolver = srv
        .block_on(lazy(|| {
            Ok::<_, ()>(actix_connect::start_resolver(
                ResolverConfig::default(),
                ResolverOpts::default(),
            ))
        }))
        .unwrap();
    let factory = srv
        .block_on(lazy(|| {
            Ok::<_, ()>(actix_connect::new_connector_factory(resolver))
        }))
        .unwrap();

    let mut conn = srv.block_on(factory.new_service(&())).unwrap();
    let con = srv
        .block_on(conn.call(Connect::with("10", srv.addr())))
        .unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}
