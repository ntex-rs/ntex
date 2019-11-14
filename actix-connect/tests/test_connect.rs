use std::io;

use actix_codec::{BytesCodec, Framed};
use actix_server_config::Io;
use actix_service::{service_fn, Service, ServiceFactory};
use actix_testing::{self as test, TestServer};
use bytes::Bytes;
use futures::SinkExt;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};

use actix_connect::Connect;

#[cfg(feature = "openssl")]
#[test]
fn test_string() {
    let srv = TestServer::with(|| {
        service_fn(|io: Io<tokio_net::tcp::TcpStream>| {
            async {
                let mut framed = Framed::new(io.into_parts().0, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = format!("localhost:{}", srv.port());
    let con = test::call_service(&mut conn, addr.into());
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[cfg(feature = "rustls")]
#[test]
fn test_rustls_string() {
    let srv = TestServer::with(|| {
        service_fn(|io: Io<tokio_net::tcp::TcpStream>| {
            async {
                let mut framed = Framed::new(io.into_parts().0, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = format!("localhost:{}", srv.port());
    let con = test::call_service(&mut conn, addr.into());
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[tokio::test]
async fn test_static_str() {
    let srv = TestServer::with(|| {
        service_fn(|io: Io<tokio_net::tcp::TcpStream>| {
            async {
                let mut framed = Framed::new(io.into_parts().0, BytesCodec);
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

#[test]
fn test_new_service() {
    let srv = TestServer::with(|| {
        service_fn(|io: Io<tokio_net::tcp::TcpStream>| {
            async {
                let mut framed = Framed::new(io.into_parts().0, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let resolver = test::block_on(async {
        actix_connect::start_resolver(ResolverConfig::default(), ResolverOpts::default())
    });
    let factory = test::block_on(async { actix_connect::new_connector_factory(resolver) });

    let mut conn = test::block_on(factory.new_service(&())).unwrap();
    let con = test::block_on(conn.call(Connect::with("10", srv.addr()))).unwrap();
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[cfg(feature = "openssl")]
#[test]
fn test_uri() {
    use http::HttpTryFrom;

    let srv = TestServer::with(|| {
        service_fn(|io: Io<tokio_net::tcp::TcpStream>| {
            async {
                let mut framed = Framed::new(io.into_parts().0, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = http::Uri::try_from(format!("https://localhost:{}", srv.port())).unwrap();
    let con = test::call_service(&mut conn, addr.into());
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}

#[cfg(feature = "rustls")]
#[test]
fn test_rustls_uri() {
    use http::HttpTryFrom;

    let srv = TestServer::with(|| {
        service_fn(|io: Io<tokio_net::tcp::TcpStream>| {
            async {
                let mut framed = Framed::new(io.into_parts().0, BytesCodec);
                framed.send(Bytes::from_static(b"test")).await?;
                Ok::<_, io::Error>(())
            }
        })
    });

    let mut conn = actix_connect::default_connector();
    let addr = http::Uri::try_from(format!("https://localhost:{}", srv.port())).unwrap();
    let con = test::call_service(&mut conn, addr.into());
    assert_eq!(con.peer_addr().unwrap(), srv.addr());
}
