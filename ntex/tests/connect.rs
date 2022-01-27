use std::io;
use std::sync::Arc;

use ntex::codec::BytesCodec;
use ntex::connect::Connect;
use ntex::io::{types::PeerAddr, Io};
use ntex::server::test_server;
use ntex::service::{fn_service, pipeline_factory, Service, ServiceFactory};
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

#[cfg(feature = "rustls")]
use tls_rustls::ServerConfig;
#[cfg(feature = "rustls")]
fn tls_acceptor() -> Arc<ServerConfig> {
    use std::fs::File;
    use std::io::BufReader;
    use rustls_pemfile::{certs, rsa_private_keys};
    use tls_rustls::{Certificate, PrivateKey};

    let cert_file = &mut BufReader::new(File::open("tests/cert.pem").unwrap());
    let key_file = &mut BufReader::new(File::open("tests/key.pem").unwrap());
    let cert_chain = certs(cert_file)
        .unwrap()
        .iter()
        .map(|c| Certificate(c.to_vec()))
        .collect();
    let keys = PrivateKey(rsa_private_keys(key_file).unwrap().remove(0));
    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain, keys)
        .unwrap();
    Arc::new(config)
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_string() {
    use ntex::server::openssl;
    use ntex_tls::openssl::PeerCert;
    use tls_openssl::{ssl::{SslConnector, SslMethod, SslVerifyMode}, x509::X509};

    let srv = test_server(|| {
        pipeline_factory(fn_service(|io: Io<_>| async move {
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
    assert_eq!(con.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    let cert = X509::from_pem(include_bytes!("cert.pem")).unwrap();
    assert_eq!(con.query::<PeerCert>().as_ref().unwrap().0.to_der().unwrap(), cert.to_der().unwrap());
    let item = con.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_read_before_error() {
    use ntex::server::openssl;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let srv = test_server(|| {
        pipeline_factory(fn_service(|io: Io<_>| async move {
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
    let io = conn.call(addr.into()).await.unwrap();
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_string() {
    use std::fs::File;
    use std::io::BufReader;
    use ntex_tls::rustls::PeerCert;
    use ntex::server::rustls;
    use rustls_pemfile::certs;
    use tls_rustls::{Certificate, ClientConfig, OwnedTrustAnchor, RootCertStore};

    let srv = test_server(|| {
        pipeline_factory(fn_service(|io: Io<_>| async move {
            let res = io.read_ready().await;
            assert!(res.is_ok());
            Ok(io)
        }))
            .and_then(rustls::Acceptor::new(tls_acceptor()))
            .and_then(fn_service(|io: Io<_>| async move {
                io.send(Bytes::from_static(b"test"), &BytesCodec)
                    .await
                    .unwrap();
                Ok::<_, std::io::Error>(())
            }))
    });

    let mut cert_store = RootCertStore::empty();
    cert_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(
        |ta| {
            OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        },
    ));
    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(cert_store)
        .with_no_client_auth();

    let conn = ntex::connect::rustls::Connector::new(config);
    let addr = format!("localhost:{}", srv.addr().port());
    let con = conn.call(addr.into()).await.unwrap();
    assert_eq!(con.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    let cert_file = &mut BufReader::new(File::open("tests/cert.pem").unwrap());
    let cert_chain: Vec<Certificate> = certs(cert_file)
        .unwrap()
        .iter()
        .map(|c| Certificate(c.to_vec()))
        .collect();
    assert_eq!(con.query::<PeerCert>().as_ref().unwrap().0, *cert_chain.first().unwrap());
    let item = con.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));
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
