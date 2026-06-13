use std::{io, rc::Rc};

use ntex::io::{Io, types::PeerAddr};
use ntex::service::{Pipeline, ServiceFactory, chain_factory, fn_service};
use ntex::{SharedCfg, codec::BytesCodec, connect::Connect};
use ntex::{server::build_test_server, server::test_server, time, util::Bytes};

#[cfg(feature = "rustls")]
mod rustls_utils;

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
    use ntex::{io::types::HttpProtocol, server::openssl};
    use ntex_tls::{openssl::PeerCert, openssl::PeerCertChain};
    use tls_openssl::{
        ssl::{SslConnector, SslMethod, SslVerifyMode},
        x509::X509,
    };

    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local_addr = tcp.local_addr().unwrap();

    let mut tcp = Some(tcp);
    let srv = build_test_server(async move |srv| {
        srv.listen("test", tcp.take().unwrap(), async |_| {
            chain_factory(openssl::SslAcceptor::new(ssl_acceptor())).and_then(
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
    let connector = builder.build();

    // ssl connector
    let conn = ntex::connect::openssl::SslConnector::new(connector.clone())
        .pipeline(SharedCfg::new("CLIENT").into())
        .await
        .unwrap();
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

    // ssl connector 2
    let conn = ntex::connect::openssl::SslConnector2::new(connector)
        .pipeline(SharedCfg::new("CLIENT").into())
        .await
        .unwrap();
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

    // error
    #[cfg(unix)]
    {
        use ntex::error::ErrorDiagnostic;

        let addr = "127.0.0.1".to_string();
        let err = conn.call(addr.into()).await.err().unwrap();
        err.backtrace().unwrap().resolver().resolve();
        assert!(
            format!("{:?}", err.debug()).contains("ntex_net::connect::service::connect::"),
            "{:#?}",
            err.debug()
        );
    }
}

#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_read_before_error() {
    use ntex::server::openssl;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let srv = test_server(async || {
        chain_factory(openssl::SslAcceptor::new(ssl_acceptor())).and_then(
            fn_service(|io: Io<_>| async move {
                io.send(Bytes::from_static(b"test"), &Rc::new(BytesCodec))
                    .await
                    .unwrap();
                time::sleep(time::Millis(50)).await;
                Ok::<_, Box<dyn std::error::Error>>(())
            })
            .map_init_err(|_| ()),
        )
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    let conn = Pipeline::new(
        ntex::connect::openssl::SslConnector::new(connector.clone())
            .create(srv.config())
            .await
            .unwrap(),
    );
    let addr = format!("127.0.0.1:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    let item = io.recv(&Rc::new(BytesCodec)).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());

    // ssl connector 2
    let conn = Pipeline::new(
        ntex::connect::openssl::SslConnector2::new(connector)
            .create(srv.config())
            .await
            .unwrap(),
    );
    let addr = format!("127.0.0.1:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    let item = io.recv(&Rc::new(BytesCodec)).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());
}

#[cfg(all(windows, feature = "openssl"))]
#[ntex::test]
async fn test_schannel_string() {
    use ntex::{io::types::HttpProtocol, server::openssl};
    use ntex_tls::schannel::{ClientConfig, PeerCert, TlsConnector, TlsConnector2};
    use tls_openssl::x509::X509;

    let srv = test_server(async || {
        chain_factory(openssl::SslAcceptor::new(ssl_acceptor())).and_then(
            fn_service(|io: Io<_>| async move {
                io.send(Bytes::from_static(b"test"), &BytesCodec)
                    .await
                    .unwrap();
                assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "test");
                Ok::<_, Box<dyn std::error::Error>>(())
            })
            .map_init_err(|_| ()),
        )
    });

    let config = ClientConfig::new().danger_accept_invalid_certs(true);

    // schannel connector
    let conn = Pipeline::new(
        TlsConnector::with_config(config.clone())
            .create(SharedCfg::new("CLIENT").into())
            .await
            .unwrap(),
    );
    let addr = format!("localhost:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    assert_eq!(
        io.query::<HttpProtocol>().get().unwrap(),
        HttpProtocol::Http1
    );
    let cert = X509::from_pem(include_bytes!("cert.pem")).unwrap();
    assert_eq!(
        io.query::<PeerCert>().as_ref().unwrap().0,
        cert.to_der().unwrap()
    );
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());

    // schannel connector 2
    let conn = Pipeline::new(
        TlsConnector2::with_config(config)
            .create(SharedCfg::default())
            .await
            .unwrap(),
    );
    let addr = format!("localhost:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    assert_eq!(
        io.query::<HttpProtocol>().get().unwrap(),
        HttpProtocol::Http1
    );
    let cert = X509::from_pem(include_bytes!("cert.pem")).unwrap();
    assert_eq!(
        io.query::<PeerCert>().as_ref().unwrap().0,
        cert.to_der().unwrap()
    );
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_string() {
    use std::{fs::File, io::BufReader};

    use ntex::{io::types::HttpProtocol, server::rustls};
    use ntex_tls::rustls::{TlsConnector, TlsConnector2};
    use ntex_tls::{rustls::PeerCert, rustls::PeerCertChain};

    let srv = test_server(async || {
        chain_factory(
            rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc()).map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            }),
        )
        .and_then(
            fn_service(|io: Io<_>| async move {
                assert!(io.query::<PeerCert<'_>>().as_ref().is_none());
                assert!(io.query::<PeerCertChain<'_>>().as_ref().is_none());
                io.send(Bytes::from_static(b"test"), &BytesCodec)
                    .await
                    .unwrap();
                assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "test");
                Ok::<_, io::Error>(())
            })
            .map_init_err(|_| ()),
        )
    });

    // tls connector
    let conn = Pipeline::new(
        TlsConnector::new(rustls_utils::tls_connector())
            .create(SharedCfg::new("CLIENT").into())
            .await
            .unwrap(),
    );
    let addr = format!("localhost:{}", srv.addr().port());

    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    assert_eq!(
        io.query::<HttpProtocol>().get().unwrap(),
        HttpProtocol::Http1
    );

    let cert_file = &mut BufReader::new(File::open("tests/cert.pem").unwrap());
    let cert_chain = rustls_pemfile::certs(cert_file)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        io.query::<PeerCert<'_>>().as_ref().unwrap().0,
        *cert_chain.first().unwrap()
    );
    assert_eq!(
        io.query::<PeerCertChain<'_>>().as_ref().unwrap().0,
        cert_chain
    );

    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.encode(Bytes::from_static(b"test"), &BytesCodec).unwrap();
    io.send_buf().unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());

    // tls connector 2
    let conn = Pipeline::new(
        TlsConnector2::new(rustls_utils::tls_connector())
            .create(SharedCfg::default())
            .await
            .unwrap(),
    );
    let addr = format!("localhost:{}", srv.addr().port());
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
    assert_eq!(
        io.query::<HttpProtocol>().get().unwrap(),
        HttpProtocol::Http1
    );
    let cert_file = &mut BufReader::new(File::open("tests/cert.pem").unwrap());
    let cert_chain = rustls_pemfile::certs(cert_file)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        io.query::<PeerCert<'_>>().as_ref().unwrap().0,
        *cert_chain.first().unwrap()
    );
    assert_eq!(
        io.query::<PeerCertChain<'_>>().as_ref().unwrap().0,
        cert_chain
    );
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));

    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    assert!(io.recv(&BytesCodec).await.unwrap().is_none());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_peer_close_notify_closes_io() {
    use std::io::{Read as _, Write as _};
    use std::{sync::Arc, time::Duration};

    use ntex::server::rustls;
    use tls_rustls::pki_types::ServerName;

    let srv = test_server(async || {
        chain_factory(
            rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc()).map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            }),
        )
        .and_then(
            fn_service(|io: Io<_>| async move {
                // echo round-trip proves the data path works
                let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
                io.send(msg, &BytesCodec).await.unwrap();

                // wait until the peer disconnects
                while let Ok(Some(_)) = io.recv(&BytesCodec).await {}
                Ok::<_, io::Error>(())
            })
            .map_init_err(|_| ()),
        )
    });

    // raw blocking rustls client
    let config = Arc::new(rustls_utils::tls_connector());
    let mut conn = tls_rustls::ClientConnection::new(
        config,
        ServerName::try_from("localhost").unwrap(),
    )
    .unwrap();
    let mut tcp = std::net::TcpStream::connect(srv.addr()).unwrap();

    // handshake + echo round-trip
    {
        let mut tls = tls_rustls::Stream::new(&mut conn, &mut tcp);
        tls.write_all(b"hello").unwrap();
        tls.flush().unwrap();
        let mut echo = [0u8; 5];
        tls.read_exact(&mut echo).unwrap();
        assert_eq!(&echo, b"hello");
    }

    // send TLS close_notify, but keep the tcp stream fully open
    // (no shutdown of the write side - tls level half-close only);
    // queue one more plaintext record so that plaintext and close_notify
    // arrive in the same batch
    conn.writer().write_all(b"bye").unwrap();
    conn.send_close_notify();
    while conn.wants_write() {
        conn.write_tls(&mut tcp).unwrap();
    }
    tcp.flush().unwrap();

    // server must close the connection in a timely manner
    tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut buf = [0u8; 4096];
    loop {
        match tcp.read(&mut buf) {
            // EOF, server closed the connection
            Ok(0) => break,
            // tls records (e.g. server's close_notify alert), keep reading
            Ok(_) => continue,
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock
                    || e.kind() == io::ErrorKind::TimedOut =>
            {
                panic!("server did not close connection after receiving close_notify: {e}");
            }
            // connection reset also indicates closure
            Err(_) => break,
        }
    }
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_shutdown_sends_close_notify() {
    use std::io::{Read, Write};
    use std::sync::Arc;

    use ntex::server::rustls;

    let srv = test_server(async || {
        chain_factory(
            rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc()).map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            }),
        )
        .and_then(
            fn_service(|io: Io<_>| async move {
                let item = io.recv(&BytesCodec).await.unwrap().unwrap();
                io.send(item, &BytesCodec).await.unwrap();
                // graceful shutdown, must deliver TLS close_notify to the peer
                io.shutdown().await.unwrap();
                Ok::<_, io::Error>(())
            })
            .map_init_err(|_| ()),
        )
    });
    let addr = srv.addr();

    // raw blocking rustls client
    let cfg = Arc::new(rustls_utils::tls_connector());
    let name = tls_rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = tls_rustls::ClientConnection::new(cfg, name).unwrap();
    let mut sock = std::net::TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut tls = tls_rustls::Stream::new(&mut conn, &mut sock);

    tls.write_all(b"hello").unwrap();
    tls.flush().unwrap();

    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");

    // keep reading until EOF; a clean Ok(0) means close_notify was received,
    // an UnexpectedEof error means the peer closed without sending close_notify
    let mut tmp = [0u8; 1024];
    loop {
        match tls.read(&mut tmp) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(e) => panic!("expected clean EOF (close_notify), got error: {e:?}"),
        }
    }
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_keyupdate_response_flushed() {
    use std::io::{Read, Write};
    use std::sync::Arc;

    use ntex::server::rustls;
    use tls_rustls::pki_types::ServerName;

    let srv = test_server(async || {
        chain_factory(
            rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc()).map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            }),
        )
        .and_then(
            fn_service(|io: Io<_>| async move {
                // echo frames, then sit idle waiting for more data
                while let Some(item) = io.recv(&BytesCodec).await.unwrap() {
                    io.send(item, &BytesCodec).await.unwrap();
                }
                Ok::<_, io::Error>(())
            })
            .map_init_err(|_| ()),
        )
    });

    // raw blocking rustls client, tls 1.3 only (KeyUpdate requires it)
    let config = tls_rustls::ClientConfig::builder_with_protocol_versions(&[
        &tls_rustls::version::TLS13,
    ])
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(rustls_utils::NoCertificateVerification))
    .with_no_client_auth();
    let mut conn = tls_rustls::ClientConnection::new(
        Arc::new(config),
        ServerName::try_from("localhost").unwrap(),
    )
    .unwrap();

    let mut tcp = std::net::TcpStream::connect(srv.addr()).unwrap();
    tcp.set_nodelay(true).unwrap();
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .unwrap();
    conn.complete_io(&mut tcp).unwrap();
    assert!(!conn.is_handshaking());

    // echo round-trip to make sure the connection is fully operational
    conn.writer().write_all(b"ping").unwrap();
    while conn.wants_write() {
        conn.write_tls(&mut tcp).unwrap();
    }
    let mut echo = Vec::new();
    while echo.len() < 4 {
        if conn.read_tls(&mut tcp).unwrap() == 0 {
            panic!("server closed connection before echo");
        }
        let state = conn.process_new_packets().unwrap();
        let n = state.plaintext_bytes_to_read();
        if n > 0 {
            let mut chunk = vec![0u8; n];
            conn.reader().read_exact(&mut chunk).unwrap();
            echo.extend_from_slice(&chunk);
        }
    }
    assert_eq!(echo, b"ping");

    // request a key update; the server must answer with its own KeyUpdate
    conn.refresh_traffic_keys().unwrap();
    while conn.wants_write() {
        conn.write_tls(&mut tcp).unwrap();
    }

    // the connection is otherwise idle; the server's KeyUpdate response
    // (generated while processing incoming data) must still reach the wire
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    match conn.read_tls(&mut tcp) {
        Ok(n) if n > 0 => {
            conn.process_new_packets().unwrap();
        }
        other => panic!(
            "server did not flush KeyUpdate response generated during read processing: {other:?}"
        ),
    }
}

#[ntex::test]
async fn test_static_str() {
    let srv = test_server(async || {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            time::sleep(time::Millis(100)).await;
            Ok::<_, io::Error>(())
        })
    });

    // original
    let conn = Pipeline::new(ntex::connect::ConnectorService::new());

    let io = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    let connect = Connect::new("127.0.0.1".to_owned());
    let conn = Pipeline::new(ntex::connect::ConnectorService::new());
    let io = conn.call(connect).await;
    assert!(io.is_err());

    // new error
    let conn = Pipeline::new(ntex::connect::ConnectorService2::new());

    let io = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    let connect = Connect::new("127.0.0.1".to_owned());
    let conn = Pipeline::new(ntex::connect::ConnectorService2::new());
    let io = conn.call(connect).await;
    assert!(io.is_err());
}

#[ntex::test]
async fn test_create() {
    let srv = test_server(async || {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });
    time::sleep(time::Millis(100)).await;

    let factory = ntex::connect::Connector::new();
    let conn = factory.pipeline(SharedCfg::default()).await.unwrap();
    let io = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    let factory = ntex::connect::Connector2::new();
    let conn = factory.pipeline(SharedCfg::default()).await.unwrap();
    let io = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[ntex::test]
async fn test_uri() {
    let srv = test_server(async || {
        fn_service(|io: Io| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });
    time::sleep(time::Millis(100)).await;

    let conn = Pipeline::new(ntex::connect::ConnectorService::default());
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    // new error
    let conn = Pipeline::new(ntex::connect::ConnectorService2::default());
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_uri() {
    use ntex::server::rustls;

    let srv = test_server(async || {
        chain_factory(
            rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc()).map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            }),
        )
        .and_then(fn_service(|io: Io<_>| async move {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        }))
    });
    time::sleep(time::Millis(50)).await;

    let conn = Pipeline::new(ntex::connect::ConnectorService::default());
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    // new error
    let conn = Pipeline::new(ntex::connect::ConnectorService2::default());
    let addr =
        ntex::http::Uri::try_from(format!("https://localhost:{}", srv.addr().port()))
            .unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

#[ntex::test]
async fn basic_connect_service() {
    let server = ntex::server::test_server(async || {
        ntex::service::fn_service(|_| async { Ok::<_, ()>(()) })
    });

    let srv = ntex_net::connect::Connector::default()
        .create(
            ntex::SharedCfg::new("T")
                .add(ntex::io::IoConfig::new().set_connect_timeout(time::Millis(5000)))
                .into(),
        )
        .await
        .unwrap();
    let result = srv.connect("").await;
    assert!(result.is_err());
    let result = srv.connect("localhost:99999").await;
    assert!(result.is_err());
    assert!(format!("{srv:?}").contains("ConnectorService"));

    let srv = ntex_net::connect::ConnectorService::default();
    let result = srv.connect(format!("{}", server.addr())).await;
    assert!(result.is_ok());

    let msg = Connect::new(format!("{}", server.addr())).set_addrs(vec![
        format!("127.0.0.1:{}", server.addr().port() - 1)
            .parse()
            .unwrap(),
        server.addr(),
    ]);
    let result = ntex_net::connect::connect(msg).await;
    assert!(result.is_ok());

    let msg = Connect::new(server.addr());
    let result = ntex_net::connect::connect(msg).await;
    assert!(result.is_ok());

    // new error
    let srv = ntex_net::connect::Connector2::default()
        .create(
            ntex::SharedCfg::new("T")
                .add(ntex::io::IoConfig::new().set_connect_timeout(time::Millis(5000)))
                .into(),
        )
        .await
        .unwrap();
    let result = srv.connect("").await;
    assert!(result.is_err());
    let result = srv.connect("localhost:99999").await;
    assert!(result.is_err());
    assert!(format!("{srv:?}").contains("ConnectorService2"));

    let srv = ntex_net::connect::ConnectorService2::default();
    let result = srv.connect(format!("{}", server.addr())).await;
    assert!(result.is_ok());
}
