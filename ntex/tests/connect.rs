use std::io;
#[cfg(feature = "openssl")]
use std::rc::Rc;

use ntex::io::{Io, types::PeerAddr};
use ntex::server::test_server;
#[cfg(feature = "openssl")]
use ntex::server::{NoConfig, build_test_server};
use ntex::service::{Pipeline, cfg::SharedCfg, service};
use ntex::{codec::BytesCodec, connect::Connect, time, util::Bytes};

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

/// STATEMENT: The OpenSSL connector exposes peer metadata and receives application data for a string address.
///
/// HOW:
/// - Connect to a local TLS server and assert `PeerAddr`, `HttpProtocol`, certificate bytes, certificate-chain length, and the received `test` payload.
/// - On Unix, connect with a missing port and assert the resolved diagnostic backtrace identifies the connection service.
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
    let srv = build_test_server(NoConfig, async move |srv| {
        srv.listen(
            "test",
            tcp.take().unwrap(),
            SharedCfg::new("SRV"),
            async |_| {
                service(openssl::SslAcceptor::new(ssl_acceptor())).and_then(
                    async move |io: Io<_>| {
                        io.send(Bytes::from_static(b"test"), &BytesCodec)
                            .await
                            .unwrap();
                        assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "test");
                        Ok::<_, io::Error>(())
                    },
                )
            },
        )
        .unwrap()
    })
    .set_addr(local_addr);

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    // ssl connector
    let conn = Pipeline::new(
        SharedCfg::new("CLIENT").build(),
        ntex::connect::openssl::SslConnector::new(connector.clone()),
    );
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

/// STATEMENT: OpenSSL delivers buffered application bytes before reporting the peer's closure.
///
/// HOW:
/// - Have a TLS server send `test` and return after a delay; read and assert that payload through an `Rc<BytesCodec>`.
/// - Send another payload and assert the next receive yields EOF.
#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_read_before_error() {
    use ntex::server::openssl;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let srv = test_server(async || {
        service(openssl::SslAcceptor::new(ssl_acceptor())).and_then(async move |io: Io<_>| {
            io.send(Bytes::from_static(b"test"), &Rc::new(BytesCodec))
                .await
                .unwrap();
            time::sleep(time::Millis(50)).await;
            Ok::<_, io::Error>(())
        })
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    let conn = Pipeline::new(
        srv.config(),
        ntex::connect::openssl::SslConnector::new(connector.clone()),
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

/// STATEMENT: OpenSSL I/O shutdown delivers TLS `close_notify` to its peer.
///
/// HOW:
/// - Echo `hello` through a server that calls `Io::shutdown` and verify the echo with a blocking OpenSSL client.
/// - Read to clean EOF and assert `ShutdownState::RECEIVED`, distinguishing TLS shutdown from a bare TCP close.
#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_shutdown_sends_close_notify() {
    use std::io::{Read, Write};

    use ntex::server::openssl;
    use tls_openssl::ssl::{ShutdownState, SslConnector, SslMethod, SslVerifyMode};

    let srv = test_server(async || {
        service(openssl::SslAcceptor::new(ssl_acceptor())).and_then(async move |io: Io<_>| {
            let item = io.recv(&BytesCodec).await.unwrap().unwrap();
            io.send(item, &BytesCodec).await.unwrap();
            // graceful shutdown, must deliver TLS close_notify to the peer
            io.shutdown().await.unwrap();
            Ok::<_, io::Error>(())
        })
    });

    // raw blocking openssl client
    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let sock = std::net::TcpStream::connect(srv.addr()).unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut tls = builder.build().connect("localhost", sock).unwrap();

    tls.write_all(b"hello").unwrap();
    tls.flush().unwrap();

    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");

    // keep reading until EOF. Depending on the OpenSSL version a TCP close
    // without close_notify is reported as an error or as `Ok(0)`, so the
    // shutdown state tells whether close_notify was received
    let mut tmp = [0u8; 1024];
    loop {
        match tls.read(&mut tmp) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(e) => panic!("expected clean EOF (close_notify), got error: {e:?}"),
        }
    }
    assert!(
        tls.get_shutdown().contains(ShutdownState::RECEIVED),
        "peer closed without close_notify"
    );
}

/// STATEMENT: OpenSSL shutdown completes promptly when the peer answers `close_notify`.
///
/// HOW:
/// - Request a one-megabyte response with a TLS 1.2 client, drain it to EOF, and answer the server's shutdown alert.
/// - Assert the server's `Io::shutdown` succeeds in under 500 ms rather than waiting for its timeout.
#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_shutdown_completes_on_peer_close_notify() {
    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    use ntex::server::openssl;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion};

    let (tx, rx) = std::sync::mpsc::channel();
    let srv = test_server(move || {
        let tx = tx.clone();
        async move {
            service(openssl::SslAcceptor::new(ssl_acceptor())).and_then(move |io: Io<_>| {
                let tx = tx.clone();
                async move {
                    io.recv(&BytesCodec).await.unwrap().unwrap();
                    io.encode(Bytes::from(vec![b'x'; 1024 * 1024]), &BytesCodec)
                        .unwrap();
                    let start = Instant::now();
                    let res = io.shutdown().await;
                    let _ = tx.send((res, start.elapsed()));
                    Ok::<_, io::Error>(())
                }
            })
        }
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    builder
        .set_max_proto_version(Some(SslVersion::TLS1_2))
        .unwrap();
    let sock = std::net::TcpStream::connect(srv.addr()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut tls = builder.build().connect("localhost", sock).unwrap();
    tls.write_all(b"go").unwrap();
    tls.flush().unwrap();

    let mut tmp = [0u8; 16 * 1024];
    while tls.read(&mut tmp).unwrap() != 0 {}
    // answer the server's close_notify
    tls.shutdown().unwrap();

    // the peer's close_notify completes the shutdown, it must not wait for
    // the shutdown timeout (1 second by default)
    let (res, elapsed) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    res.unwrap();
    assert!(
        elapsed < Duration::from_millis(500),
        "shutdown took {elapsed:?}"
    );
}

/// STATEMENT: A zero OpenSSL handshake timeout disables the deadline.
///
/// HOW:
/// - Set `TlsConfig` handshake timeout to `Seconds(0)` and delay the client handshake by 200 ms after connecting.
/// - Complete TLS negotiation and assert `hello` is echoed.
#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_accept_zero_handshake_timeout() {
    use std::time::Duration;

    use ntex::{server::TestServerBuilder, server::openssl, time::Seconds};
    use ntex_tls::TlsConfig;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    // zero disables the handshake timeout
    let srv = TestServerBuilder::new(async || {
        service(openssl::SslAcceptor::new(ssl_acceptor())).and_then(async move |io: Io<_>| {
            let item = io.recv(&BytesCodec).await.unwrap().unwrap();
            io.send(item, &BytesCodec).await.unwrap();
            Ok::<_, io::Error>(())
        })
    })
    .config(SharedCfg::new("SRV").add(TlsConfig::new().set_handshake_timeout(Seconds(0))))
    .start();

    let sock = std::net::TcpStream::connect(srv.addr()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // a handshake that is not done within a timer tick
    std::thread::sleep(Duration::from_millis(200));

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let mut tls = builder.build().connect("localhost", sock).unwrap();

    use std::io::{Read, Write};
    tls.write_all(b"hello").unwrap();
    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");
}

/// STATEMENT: The OpenSSL acceptor succeeds when the final handshake flight is followed immediately by EOF.
///
/// HOW:
/// - Repeat 20 TLS connections and half-close the client TCP write side immediately after negotiation.
/// - Assert each server accept succeeds and its first application receive returns EOF, even if EOF and handshake bytes arrive together.
#[cfg(feature = "openssl")]
#[ntex::test]
async fn test_openssl_accept_handshake_then_eof() {
    use std::time::Duration;

    use ntex::server::openssl;
    use tls_openssl::ssl::{SslAcceptor, SslConnector, SslFiletype, SslMethod, SslVerifyMode};

    fn tls13_acceptor() -> SslAcceptor {
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        builder
            .set_private_key_file("./tests/key.pem", SslFiletype::PEM)
            .unwrap();
        builder
            .set_certificate_chain_file("./tests/cert.pem")
            .unwrap();
        builder.build()
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let srv = test_server(move || {
        let tx = tx.clone();
        async move {
            let tx2 = tx.clone();
            service(openssl::SslAcceptor::new(tls13_acceptor()))
                .map_err(move |e| {
                    let _ = tx2.send(Err(e.to_string()));
                    e
                })
                .and_then(move |io: Io<_>| {
                    let tx = tx.clone();
                    async move {
                        let _ = tx.send(Ok(io.recv(&BytesCodec).await.is_ok_and(|v| v.is_none())));
                        Ok::<_, io::Error>(())
                    }
                })
        }
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    // the peer's last handshake flight and its eof are often read together
    for _ in 0..20 {
        let sock = std::net::TcpStream::connect(srv.addr()).unwrap();
        let tls = connector.connect("localhost", sock).unwrap();
        tls.get_ref().shutdown(std::net::Shutdown::Write).unwrap();

        let res = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(res, Ok(true), "handshake followed by eof must succeed");
    }
}

/// STATEMENT: A zero rustls handshake timeout disables the deadline.
///
/// HOW:
/// - Set the rustls acceptor's `TlsConfig` timeout to `Seconds(0)` and delay an OpenSSL client handshake by 200 ms.
/// - Complete negotiation and assert `hello` is echoed.
#[cfg(all(feature = "rustls", feature = "openssl"))]
#[ntex::test]
async fn test_rustls_accept_zero_handshake_timeout() {
    use std::io::{Read, Write};
    use std::time::Duration;

    use ntex::{server::TestServerBuilder, server::rustls, time::Seconds};
    use ntex_tls::TlsConfig;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    // zero disables the handshake timeout
    let srv = TestServerBuilder::new(async || {
        service(rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc())).and_then(
            async move |io: Io<_>| {
                let item = io.recv(&BytesCodec).await.unwrap().unwrap();
                io.send(item, &BytesCodec).await.unwrap();
                Ok::<_, io::Error>(())
            },
        )
    })
    .config(SharedCfg::new("SRV").add(TlsConfig::new().set_handshake_timeout(Seconds(0))))
    .start();

    let sock = std::net::TcpStream::connect(srv.addr()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // a handshake that is not done within a timer tick
    std::thread::sleep(Duration::from_millis(200));

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let mut tls = builder.build().connect("localhost", sock).unwrap();

    tls.write_all(b"hello").unwrap();
    let mut echo = [0u8; 5];
    tls.read_exact(&mut echo).unwrap();
    assert_eq!(&echo, b"hello");
}

/// STATEMENT: The rustls acceptor succeeds when the final handshake flight is followed immediately by EOF.
///
/// HOW:
/// - Repeat 20 OpenSSL client connections and immediately half-close each TCP write side after negotiation.
/// - Assert the server reports successful acceptance followed by application EOF, including coalesced handshake and EOF delivery.
#[cfg(all(feature = "rustls", feature = "openssl"))]
#[ntex::test]
async fn test_rustls_accept_handshake_then_eof() {
    use std::time::Duration;

    use ntex::server::rustls;
    use tls_openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let (tx, rx) = std::sync::mpsc::channel();
    let srv = test_server(move || {
        let tx = tx.clone();
        async move {
            let tx2 = tx.clone();
            service(rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc()))
                .map_err(move |e| {
                    let _ = tx2.send(Err(e.to_string()));
                    e
                })
                .and_then(move |io: Io<_>| {
                    let tx = tx.clone();
                    async move {
                        let _ = tx.send(Ok(io.recv(&BytesCodec).await.is_ok_and(|v| v.is_none())));
                        Ok::<_, io::Error>(())
                    }
                })
        }
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let connector = builder.build();

    // the peer's last handshake flight and its eof are often read together
    for _ in 0..20 {
        let sock = std::net::TcpStream::connect(srv.addr()).unwrap();
        let tls = connector.connect("localhost", sock).unwrap();
        tls.get_ref().shutdown(std::net::Shutdown::Write).unwrap();

        let res = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(res, Ok(true), "handshake followed by eof must succeed");
    }
}

/// STATEMENT: The Schannel connector exposes peer metadata and exchanges data with an OpenSSL server.
///
/// HOW:
/// - Connect using a `localhost` string address and assert `PeerAddr`, `HttpProtocol::Http1`, and the certificate DER bytes.
/// - Send `test` and assert the echoed payload matches.
#[cfg(all(windows, feature = "openssl"))]
#[ntex::test]
async fn test_schannel_string() {
    use ntex::{io::types::HttpProtocol, server::openssl};
    use ntex_tls::schannel::{ClientConfig, PeerCert, TlsConnector};
    use tls_openssl::x509::X509;

    let srv = test_server(async || {
        service(openssl::SslAcceptor::new(ssl_acceptor())).and_then(async move |io: Io<_>| {
            let item = io.recv(&BytesCodec).await.unwrap().unwrap();
            io.send(item, &BytesCodec).await.unwrap();
            Ok::<_, io::Error>(())
        })
    });

    let config = ClientConfig::new().danger_accept_invalid_certs(true);

    // schannel connector
    let conn = Pipeline::new(
        SharedCfg::new("CLIENT").into(),
        TlsConnector::with_config(config.clone()),
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
    io.send(Bytes::from_static(b"test"), &BytesCodec)
        .await
        .unwrap();
    let item = io.recv(&BytesCodec).await.unwrap().unwrap();
    assert_eq!(item, Bytes::from_static(b"test"));
}

/// STATEMENT: The rustls connector exposes peer metadata and drains data before EOF.
///
/// HOW:
/// - Assert client-side address, protocol, certificate, and certificate-chain metadata; assert the unauthenticated server sees no client certificates.
/// - Receive `test`, queue and flush a reply, and assert the next receive is EOF.
#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_string() {
    use std::{fs::File, io::BufReader};

    use ntex::{Service, io::types::HttpProtocol, server::rustls};
    use ntex_tls::rustls::{PeerCert, PeerCertChain, TlsConnector};

    let srv = test_server(async || {
        rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc())
            .map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            })
            .and_then(async move |io: Io<_>| {
                assert!(io.query::<PeerCert<'_>>().as_ref().is_none());
                assert!(io.query::<PeerCertChain<'_>>().as_ref().is_none());
                io.send(Bytes::from_static(b"test"), &BytesCodec)
                    .await
                    .unwrap();
                assert_eq!(io.recv(&BytesCodec).await.unwrap().unwrap(), "test");
                Ok::<_, io::Error>(())
            })
    });

    // tls connector
    let conn = Pipeline::new(
        SharedCfg::new("CLIENT").build(),
        TlsConnector::new(rustls_utils::tls_connector()),
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
}

/// STATEMENT: Receiving rustls `close_notify` closes I/O even when the peer keeps TCP open.
///
/// HOW:
/// - Verify an echo round trip, then queue plaintext and `close_notify` together on a blocking rustls client without half-closing TCP.
/// - Read raw TLS bytes until EOF or reset and fail if the server does not close within the read timeout.
#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_peer_close_notify_closes_io() {
    use std::io::{Read as _, Write as _};
    use std::{sync::Arc, time::Duration};

    use ntex::{Service, server::rustls};
    use tls_rustls::pki_types::ServerName;

    let srv = test_server(async || {
        rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc())
            .map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            })
            .and_then(async move |io: Io<_>| {
                // echo round-trip proves the data path works
                let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
                io.send(msg, &BytesCodec).await.unwrap();

                // wait until the peer disconnects
                while let Ok(Some(_)) = io.recv(&BytesCodec).await {}
                Ok::<_, io::Error>(())
            })
    });

    // raw blocking rustls client
    let config = Arc::new(rustls_utils::tls_connector());
    let mut conn =
        tls_rustls::ClientConnection::new(config, ServerName::try_from("localhost").unwrap())
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

    // set before close_notify, the server may close the socket right after it
    tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();

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
    let mut buf = [0u8; 4096];
    loop {
        match tcp.read(&mut buf) {
            // EOF, server closed the connection
            Ok(0) => break,
            // tls records (e.g. server's close_notify alert), keep reading
            Ok(_) => continue,
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                panic!("server did not close connection after receiving close_notify: {e}");
            }
            // connection reset also indicates closure
            Err(_) => break,
        }
    }
}

/// STATEMENT: Rustls I/O shutdown sends `close_notify` instead of ending with an unclean TCP EOF.
///
/// HOW:
/// - Echo `hello` through a server that calls `Io::shutdown` and assert the blocking rustls client receives the echo.
/// - Continue reading and require clean EOF rather than `UnexpectedEof` or another error.
#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_shutdown_sends_close_notify() {
    use std::io::{Read, Write};
    use std::sync::Arc;

    use ntex::{Service, server::rustls};

    let srv = test_server(async || {
        rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc())
            .map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            })
            .and_then(async move |io: Io<_>| {
                let item = io.recv(&BytesCodec).await.unwrap().unwrap();
                io.send(item, &BytesCodec).await.unwrap();
                // graceful shutdown, must deliver TLS close_notify to the peer
                io.shutdown().await.unwrap();
                Ok::<_, io::Error>(())
            })
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

/// STATEMENT: Rustls shutdown waits for the peer's TLS shutdown response rather than completing on its own alert.
///
/// HOW:
/// - Read the server's `close_notify`, delay 300 ms, and answer it while keeping the TCP socket open.
/// - Assert `Io::shutdown` succeeds after at least 300 ms but before 900 ms, ahead of the default timeout.
#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_shutdown_waits_for_peer_close_notify() {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use ntex::{Service, server::rustls};

    let (tx, rx) = std::sync::mpsc::channel();
    let srv = test_server(move || {
        let tx = tx.clone();
        async move {
            rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc()).and_then(move |io: Io<_>| {
                let tx = tx.clone();
                async move {
                    io.recv(&BytesCodec).await.unwrap().unwrap();
                    let start = Instant::now();
                    let res = io.shutdown().await;
                    let _ = tx.send((res, start.elapsed()));
                    Ok::<_, io::Error>(())
                }
            })
        }
    });

    let cfg = Arc::new(rustls_utils::tls_connector());
    let name = tls_rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = tls_rustls::ClientConnection::new(cfg, name).unwrap();
    let mut sock = std::net::TcpStream::connect(srv.addr()).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut tls = tls_rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(b"go").unwrap();
    tls.flush().unwrap();

    // server's close_notify
    let mut tmp = [0u8; 1024];
    assert_eq!(tls.read(&mut tmp).unwrap(), 0);

    // answer it late, the server must still be waiting for it
    std::thread::sleep(Duration::from_millis(300));
    tls.conn.send_close_notify();
    tls.flush().unwrap();

    // socket stays open, only the close_notify can complete the shutdown
    // before the shutdown timeout (1 second by default)
    let (res, elapsed) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    res.unwrap();
    assert!(
        elapsed >= Duration::from_millis(300) && elapsed < Duration::from_millis(900),
        "shutdown took {elapsed:?}"
    );
}

/// STATEMENT: Rustls flushes a TLS 1.3 key-update response generated during read processing on an otherwise idle connection.
///
/// HOW:
/// - Complete a TLS 1.3 handshake, exchange and verify `ping`, then request a traffic-key update.
/// - Without sending more application data, require nonempty TLS input from the server and process it successfully.
#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_keyupdate_response_flushed() {
    use std::io::{Read, Write};
    use std::sync::Arc;

    use ntex::{Service, server::rustls};
    use tls_rustls::pki_types::ServerName;

    let srv = test_server(async || {
        rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc())
            .map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            })
            .and_then(async move |io: Io<_>| {
                // echo frames, then sit idle waiting for more data
                while let Some(item) = io.recv(&BytesCodec).await.unwrap() {
                    io.send(item, &BytesCodec).await.unwrap();
                }
                Ok::<_, io::Error>(())
            })
    });

    // raw blocking rustls client, tls 1.3 only (KeyUpdate requires it)
    let config =
        tls_rustls::ClientConfig::builder_with_protocol_versions(&[&tls_rustls::version::TLS13])
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
    tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    conn.refresh_traffic_keys().unwrap();
    while conn.wants_write() {
        conn.write_tls(&mut tcp).unwrap();
    }

    // the connection is otherwise idle; the server's KeyUpdate response
    // (generated while processing incoming data) must still reach the wire
    match conn.read_tls(&mut tcp) {
        Ok(n) if n > 0 => {
            conn.process_new_packets().unwrap();
        }
        other => panic!(
            "server did not flush KeyUpdate response generated during read processing: {other:?}"
        ),
    }
}

/// STATEMENT: An explicit socket address overrides an unresolved host string, while a host without a port fails.
///
/// HOW:
/// - Connect with `Connect::with("10", server_addr)` and assert the reported `PeerAddr`.
/// - Connect using `127.0.0.1` without a port and assert an error.
#[ntex::test]
async fn test_static_str() {
    let srv = test_server(async || {
        service(async move |io: Io| {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            time::sleep(time::Millis(100)).await;
            Ok::<_, io::Error>(())
        })
    });

    // original
    let conn = Pipeline::new(SharedCfg::default(), ntex::connect::Connector::new());

    let io = conn.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());

    let connect = Connect::new("127.0.0.1".to_owned());
    let conn = Pipeline::new(SharedCfg::default(), ntex::connect::Connector::new());
    let io = conn.call(connect).await;
    assert!(io.is_err());
}

/// STATEMENT: A connector pipeline connects using an explicitly supplied socket address.
///
/// HOW:
/// - Start a byte-sending TCP server and call a new `Connector` pipeline with `Connect::with`.
/// - Assert the connected I/O reports the server's address through `PeerAddr`.
#[ntex::test]
async fn test_create() {
    let srv = test_server(async || {
        service(async move |io: Io| {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });
    time::sleep(time::Millis(100)).await;

    let svc = Pipeline::new(SharedCfg::default(), ntex::connect::Connector::new());
    let io = svc.call(Connect::with("10", srv.addr())).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

/// STATEMENT: A plain connector resolves the host and port from a URL.
///
/// HOW:
/// - Start a local TCP server and construct an HTTPS `Url` containing its port.
/// - Connect through `Connector` and assert `PeerAddr` matches the server address.
#[ntex::test]
async fn test_uri() {
    let srv = test_server(async || {
        service(async move |io: Io| {
            io.send(Bytes::from_static(b"test"), &BytesCodec)
                .await
                .unwrap();
            Ok::<_, io::Error>(())
        })
    });
    time::sleep(time::Millis(100)).await;

    let conn = Pipeline::new(SharedCfg::default(), ntex::connect::Connector::new());
    let addr =
        ntex::url::Url::try_from(format!("https://localhost:{}", srv.addr().port())).unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

/// STATEMENT: A connector establishes the TCP connection specified by a URL pointing at a rustls listener.
///
/// HOW:
/// - Start a rustls acceptor and construct an HTTPS `Url` using its local port.
/// - Call the default `Connector` and assert the returned I/O has the expected `PeerAddr`.
#[cfg(feature = "rustls")]
#[ntex::test]
async fn test_rustls_uri() {
    use ntex::{Service, server::rustls};

    let srv = test_server(async || {
        rustls::TlsAcceptor::new(rustls_utils::tls_acceptor_arc())
            .map_err(|e| {
                log::error!("tls negotiation is failed: {e:?}");
                e
            })
            .and_then(async move |io: Io<_>| {
                io.send(Bytes::from_static(b"test"), &BytesCodec)
                    .await
                    .unwrap();
                Ok::<_, io::Error>(())
            })
    });
    time::sleep(time::Millis(50)).await;

    let conn = Pipeline::new(SharedCfg::default(), ntex::connect::Connector::default());
    let addr =
        ntex::url::Url::try_from(format!("https://localhost:{}", srv.addr().port())).unwrap();
    let io = conn.call(addr.into()).await.unwrap();
    assert_eq!(io.query::<PeerAddr>().get().unwrap(), srv.addr().into());
}

/// STATEMENT: Connector validation rejects invalid destinations and falls back across supplied addresses.
///
/// HOW:
/// - Assert empty and out-of-range-port destinations fail, the debug label contains `Connector`, and a valid server address connects.
/// - Supply an unavailable address followed by the server address and require success; also verify `connect_with` using explicit configuration.
#[ntex::test]
async fn basic_connect_service() {
    let server = ntex::server::test_server(async || service(async |_| Ok::<_, ()>(())));

    let cfg = ntex::SharedCfg::new("T")
        .add(ntex::io::IoConfig::new().set_connect_timeout(time::Millis(5000)))
        .build();

    let srv = ntex_net::connect::Connector::new();
    let result = srv.connect("", &cfg).await;
    assert!(result.is_err());
    let result = srv.connect("localhost:99999", &cfg).await;
    assert!(result.is_err());
    assert!(format!("{srv:?}").contains("Connector"));

    let srv = ntex_net::connect::Connector::new();
    let result = srv.connect(format!("{}", server.addr()), &cfg).await;
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
    let result = ntex_net::connect::connect_with(msg, &cfg).await;
    assert!(result.is_ok());
}
