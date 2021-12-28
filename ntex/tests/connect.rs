use std::io;

use ntex::codec::BytesCodec;
use ntex::connect::Connect;
use ntex::io::{types::PeerAddr, Io};
use ntex::server::test_server;
use ntex::service::{fn_service, Service, ServiceFactory};
use ntex::util::Bytes;

#[cfg(feature = "openssl")]
fn ssl_acceptor() -> tls_openssl::ssl::SslAcceptor {
    use tls_openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};

    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder
        .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
        .unwrap();
    builder
        .set_certificate_chain_file("./tests/cert.pem")
        .unwrap();
    builder.build()
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_string() {
    use ntex::server::openssl;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let srv = test_server(|| {
        ntex::pipeline_factory(fn_service(|io: Io<_>| async move {
            let res = io.read_ready().await;
            assert!(res.is_ok());
            Ok(io)
        }))
        .and_then(openssl::Acceptor::new(ssl_acceptor()))
        .and_then(fn_service(|io: Io<_>| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, Box<dyn std::error::Error>>(())
        }))
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);

    let conn = ntex::connect::openssl::Connector::new(builder.build());
    let addr = format!("127.0.0.1:{}", srv.addr().port());
    let con = conn.call(addr.into()).await.unwrap();
    let item = con.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_read_before_error() {
    env_logger::init();
    use ntex::server::openssl;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let srv = test_server(|| {
        ntex::pipeline_factory(fn_service(|io: Io<_>| async move {
            let res = io.read_ready().await;
            assert!(res.is_ok());
            Ok(io)
        }))
        .and_then(openssl::Acceptor::new(ssl_acceptor()))
        .and_then(fn_service(|io: Io<_>| async move {
            log::info!("ssl handshake completed");
            io.encode(Bytes::from_static(b"test"), &BytesCodec).unwrap();
            // ntex::time::sleep(ntex::time::Millis(1000)).await;
            io.shutdown().await.unwrap();
            Ok::<_, Box<dyn std::error::Error>>(())
        }))
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);

    let conn = ntex::connect::openssl::Connector::new(builder.build());
    let addr = format!("127.0.0.1:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_string() {
    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let conn = ntex::connect::Connector::default();
    let addr = format!("localhost:{}", srv.addr().port());
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[ntex::test]
async fn test_static_str() {
    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let conn = ntex::connect::Connector::new();

    let con = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(con.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    let connect = Connect::new("127.0.0.1".to_owned());
    let conn = ntex::connect::Connector::new();
    let con = conn.call(connect).await;
    assert!(con.is_err());
}

#[ntex::test]
async fn test_new_service() {
    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let factory = ntex::connect::Connector::new();
    let conn = factory.new_service(()).await.unwrap();
    let con = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(con.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_uri() {
    use std::convert::TryFrom;

    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let conn = ntex::connect::Connector::default();
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_uri() {
    use std::convert::TryFrom;

    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let conn = ntex::connect::Connector::default();
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}
