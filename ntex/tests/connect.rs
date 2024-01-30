use std::{io, rc::Rc, sync::Arc};

use ntex::codec::BytesCodec;
use ntex::connect::Connect;
use ntex::io::{types::PeerAddr, Io};
use ntex::service::{chain_factory, fn_service, Pipeline, ServiceFactory};
use ntex::{server::build_test_server, server::test_server, time, util::Bytes};

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
    use rustls_pemfile::{certs, pkcs8_private_keys};
    use std::fs::File;
    use std::io::BufReader;
    use tls_rustls::{Certificate, PrivateKey};

    let cert_file = &mut BufReader::new(File::open("tests/cert.pem").unwrap());
    let key_file = &mut BufReader::new(File::open("tests/key.pem").unwrap());
    let cert_chain = certs(cert_file)
        .unwrap()
        .iter()
        .map(|c| Certificate(c.to_vec()))
        .collect();
    let key = PrivateKey(pkcs8_private_keys(key_file).unwrap().remove(0));
    let config = ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .unwrap();
    Arc::new(config)
}

mod danger {
    use std::time::SystemTime;
    use tls_rustls::{Certificate, ServerName};

    pub struct NoCertificateVerification {}

    impl tls_rustls::client::ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &Certificate,
            _intermediates: &[Certificate],
            _server_name: &ServerName,
            _scts: &mut dyn Iterator<Item = &[u8]>,
            _ocsp_response: &[u8],
            _now: SystemTime,
        ) -> Result<tls_rustls::client::ServerCertVerified, tls_rustls::Error> {
            Ok(tls_rustls::client::ServerCertVerified::assertion())
        }
    }
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_string() {
    use ntex::{io::types::HttpProtocol, server::openssl};
    use ntex_tls::{openssl::PeerCert, openssl::PeerCertChain};
    use tls_openssl::{
        ssl::{SslConnector, SslMethod, SslVerifyMode},
        x509::X509,
    };

    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local_addr = tcp.local_addr().unwrap();

    let tcp = Some(tcp);
    let srv = build_test_server(move |srv| {
        srv.listen("test", tcp.unwrap(), |_| {
            chain_factory(
                fn_service(|io: Io<_>| async move {
                    let res = io.read_ready().await;
                    assert!(res.is_ok());
                    Ok(io)
                })
                .map_init_err(|_| ()),
            )
            .and_then(openssl::SslAcceptor::new(ssl_acceptor()))
            .and_then(
                fn_service(|io: Io<_>| async move {
                    io.send(Bytes::from_static(b"test"), &BytesCodec)
                        .await
                        .unwrap();
                    assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "test");
                    Ok::<_, Box<dyn std::error::Error>>(())
                })
                .map_init_err(|_| ()),
            )
        })
        .unwrap()
    })
    .set_addr(local_addr);

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);

    let conn = Pipeline::new(ntex::connect::openssl::Connector::new(builder.build()));
    let addr = format!("127.0.0.1:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    assert_eq!(
        io.query::<HttpProtocol>().get().unwrap(),
        HttpProtocol::Http1
    );
    let cert = X509::from_pem(include_bytes!("cert.pem")).unwrap();
    assert_eq!(
        io.query::<PeerCert>().as_ref().unwrap().0.to_der().unwrap(),
        cert.to_der().unwrap()
    );
    assert_eq!(io.query::<PeerCertChain>().as_ref().unwrap().0.len(), 1);
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_read_before_error() {
    use ntex::server::openssl;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let srv = test_server(|| {
        chain_factory(
            fn_service(|io: Io<_>| async move {
                let res = io.read_ready().await;
                assert!(res.is_ok());
                Ok(io)
            })
            .map_init_err(|_| ()),
        )
        .and_then(openssl::SslAcceptor::new(ssl_acceptor()))
        .and_then(
            fn_service(|io: Io<_>| async move {
                io.send(Bytes::from_static(b"test"), &Rc::new(BytesCodec))
                    .await
                    .unwrap();
                time::sleep(time::Millis(100)).await;
                Ok::<_, Box<dyn std::error::Error>>(())
            })
            .map_init_err(|_| ()),
        )
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);

    let conn = Pipeline::new(ntex::connect::openssl::Connector::new(builder.build()));
    let addr = format!("127.0.0.1:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    let item = io.recv(&Rc::new(BytesCodec)).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_string() {
    use ntex::{io::types::HttpProtocol, server::rustls};
    use ntex_tls::{rustls::PeerCert, rustls::PeerCertChain};
    use rustls_pemfile::certs;
    use std::fs::File;
    use std::io::BufReader;
    use tls_rustls::{Certificate, ClientConfig};

    let srv = test_server(|| {
        chain_factory(
            fn_service(|io: Io<_>| async move {
                let res = io.read_ready().await;
                assert!(res.is_ok());
                Ok(io)
            })
            .map_init_err(|_| ()),
        )
        .and_then(rustls::TlsAcceptor::new(tls_acceptor()))
        .and_then(
            fn_service(|io: Io<_>| async move {
                assert!(io.query::<PeerCert>().as_ref().is_none());
                assert!(io.query::<PeerCertChain>().as_ref().is_none());
                io.send(Bytes::from_static(b"test"), &BytesCodec)
                    .await
                    .unwrap();
                assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "test");
                Ok::<_, std::io::Error>(())
            })
            .map_init_err(|_| ()),
        )
    });

    let config = ClientConfig::builder()
        .with_safe_defaults()
        .with_custom_certificate_verifier(Arc::new(danger::NoCertificateVerification {}))
        .with_no_client_auth();

    let conn = Pipeline::new(ntex::connect::rustls::Connector::new(config));
    let addr = format!("localhost:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    assert_eq!(
        io.query::<HttpProtocol>().get().unwrap(),
        HttpProtocol::Http1
    );
    let cert_file = &mut BufReader::new(File::open("tests/cert.pem").unwrap());
    let cert_chain: Vec<Certificate> = certs(cert_file)
        .unwrap()
        .iter()
        .map(|c| Certificate(c.to_vec()))
        .collect();
    assert_eq!(
        io.query::<PeerCert>().as_ref().unwrap().0,
        *cert_chain.first().unwrap()
    );
    assert_eq!(io.query::<PeerCertChain>().as_ref().unwrap().0, cert_chain);
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());
}

#[ntex::test]
async fn test_static_str() {
    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            time::sleep(time::Millis(100)).await;
            Ok::<_, io::Error>(())
        })
    });

    let conn = Pipeline::new(ntex::connect::Connector::new());

    let io = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    let connect = Connect::new("127.0.0.1".to_owned());
    let conn = Pipeline::new(ntex::connect::Connector::new());
    let io = conn.call(connect).await;
    assert!(io.is_err());
}

#[ntex::test]
async fn test_create() {
    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let factory = ntex::connect::Connector::new();
    let conn = factory.pipeline(()).await.unwrap();
    let io = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_uri() {
    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let conn = Pipeline::new(ntex::connect::Connector::default());
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_uri() {
    let srv = test_server(|| {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let conn = Pipeline::new(ntex::connect::Connector::default());
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}
