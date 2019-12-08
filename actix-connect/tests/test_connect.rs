use std::io;

use actix_codec::{BytesCodec, Framed};
use actix_rt::net::TcpStream;
use actix_service::{fn_service, Service, ServiceFactory};
use actix_testing::TestServer;
use bytes::Bytes;
use futures::SinkExt;

use actix_connect::resolver::{ResolverConfig, ResolverOpts};
use actix_connect::Connect;

#[cfg(feature = "openssl")]
#[actix_rt::test]
async fn test_string() {
    let srv = TestServer::with(|| {
        fn_service(|io: TcpStream| {
            async {
                let mut framed = Framed::new(io, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = format!("localhost:{}", srv.port());
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[cfg(feature = "rustls")]
#[actix_rt::test]
async fn test_rustls_string() {
    let srv = TestServer::with(|| {
        fn_service(|io: TcpStream| {
            async {
                let mut framed = Framed::new(io, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = format!("localhost:{}", srv.port());
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[actix_rt::test]
async fn test_static_str() {
    let srv = TestServer::with(|| {
        fn_service(|io: TcpStream| {
            async {
                let mut framed = Framed::new(io, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let resolver = actix_connect::start_default_resolver();
    let mut conn = actix_connect::new_connector(resolver.clone());

    let con = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());

    let connect = Connect::new(srv.host().to_owned());
    let mut conn = actix_connect::new_connector(resolver);
    let con = conn.call(connect).await;
    assert!(con.is_err());
}

#[actix_rt::test]
async fn test_new_service() {
    let srv = TestServer::with(|| {
        fn_service(|io: TcpStream| {
            async {
                let mut framed = Framed::new(io, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let resolver =
        actix_connect::start_resolver(ResolverConfig::default(), ResolverOpts::default());

    let factory = actix_connect::new_connector_factory(resolver);

    let mut conn = factory.new_service(()).await.unwrap();
    let con = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[cfg(feature = "openssl")]
#[actix_rt::test]
async fn test_uri() {
    use std::convert::TryFrom;

    let srv = TestServer::with(|| {
        fn_service(|io: TcpStream| {
            async {
                let mut framed = Framed::new(io, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = http::Uri::try_from(format!("https://localhost:{}", srv.port())).unwrap();
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[cfg(feature = "rustls")]
#[actix_rt::test]
async fn test_rustls_uri() {
    use std::convert::TryFrom;

    let srv = TestServer::with(|| {
        fn_service(|io: TcpStream| {
            async {
                let mut framed = Framed::new(io, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = http::Uri::try_from(format!("https://localhost:{}", srv.port())).unwrap();
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}
