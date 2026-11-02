//! TLS services and filters backed by rustls.
//!
//! [`TlsAcceptor`] and [`TlsConnector`] apply [`crate::TlsConfig`] to service
//! handshakes. [`TlsClientFilter::create`] and [`TlsServerFilter::create`] upgrade
//! existing I/O directly; only the server constructor takes an explicit timeout.
//! Neither direct constructor applies acceptor concurrency backpressure.
//!
//! Configure certificates, verification, ALPN, and the crypto provider on the
//! supplied rustls configuration. The filters do not install trust roots or
//! override those settings.
//!
//! Queries expose [`PeerCert`], [`PeerCertChain`], their backend-independent DER
//! equivalents, [`crate::TlsVersion`], [`crate::TlsCipher`], and
//! [`ntex_io::types::HttpProtocol`]. Server filters additionally expose received
//! [`crate::Servername`]. Only `h2` selects HTTP/2; absent or other ALPN values
//! report HTTP/1.
use tls_rustls::pki_types::CertificateDer;

mod accept;
mod client;
mod connect;
mod identity;
mod server;
mod stream;

pub use self::accept::TlsAcceptor;
pub use self::client::TlsClientFilter;
pub use self::connect::TlsConnector;
pub use self::identity::{certified_key, client_certified_key};
pub use self::server::TlsServerFilter;

use self::stream::Stream;

/// The peer's end-entity certificate as a rustls certificate value.
///
/// Filter queries return an owned certificate. No value is returned if the
/// peer did not supply a certificate.
#[derive(Debug)]
pub struct PeerCert<'a>(
    /// DER-encoded certificate; queried values own their bytes.
    pub CertificateDer<'a>,
);

/// The certificate chain presented by the peer, with its leaf first.
///
/// Filter queries return owned certificate values. The chain is not necessarily
/// complete through a trust root; verification follows the rustls configuration.
#[derive(Debug)]
pub struct PeerCertChain<'a>(
    /// Certificates in the order reported by rustls.
    pub Vec<CertificateDer<'a>>,
);

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, io, rc::Rc, sync::Arc};

    use ntex::codec::BytesCodec;
    use ntex_bytes::Bytes;
    use ntex_error::Error;
    use ntex_io::{Io, Layer, testing::IoTest, types::HttpProtocol};
    use ntex_net::connect::{Connect, ConnectError, Connector};
    use ntex_service::{Pipeline, cfg::SharedCfg, fn_service};
    use ntex_util::{future::join, future::lazy, time::Millis, time::sleep};
    use tls_rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tls_rustls::pki_types::{ServerName, UnixTime};
    use tls_rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme};

    use super::*;
    use crate::{MAX_SSL_ACCEPT_COUNTER, Servername, TlsCipher, TlsConfig, TlsVersion};

    const CERT: &[u8] = include_bytes!("../../examples/cert.pem");
    const KEY: &[u8] = include_bytes!("../../examples/key.pem");

    #[derive(Debug)]
    struct NoVerify;

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, tls_rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, tls_rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, tls_rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            tls_rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    fn server_config(alpn: bool) -> Arc<ServerConfig> {
        let certs = rustls_pemfile::certs(&mut &CERT[..])
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls_pemfile::private_key(&mut &KEY[..]).unwrap().unwrap();
        let mut cfg = ServerConfig::builder_with_provider(
            tls_rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        if alpn {
            cfg.alpn_protocols = vec![b"h2".to_vec()];
        }
        Arc::new(cfg)
    }

    fn client_config(alpn: bool) -> Arc<ClientConfig> {
        let mut cfg = ClientConfig::builder_with_provider(
            tls_rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
        if alpn {
            cfg.alpn_protocols = vec![b"h2".to_vec()];
        }
        Arc::new(cfg)
    }

    fn pair() -> (IoTest, IoTest) {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1 << 20);
        server.remote_buffer_cap(1 << 20);
        (client, server)
    }

    fn tls_cfg(timeout: Millis) -> TlsConfig {
        TlsConfig {
            handshake_timeout: timeout,
            ..TlsConfig::default()
        }
    }

    type ClientIo = Io<Layer<TlsClientFilter>>;
    type ServerIo = Io<Layer<TlsServerFilter>>;

    async fn handshake_pair(alpn: bool, host: &str) -> (ClientIo, ServerIo) {
        let (client, server) = pair();
        let (client, server) = join(
            TlsClientFilter::create(
                Io::new(client, SharedCfg::new("CLI")),
                client_config(alpn),
                ServerName::try_from(host.to_string()).unwrap(),
            ),
            TlsServerFilter::create(
                Io::new(server, SharedCfg::new("SRV")),
                server_config(alpn),
                Millis(5_000),
            ),
        )
        .await;
        (client.unwrap(), server.unwrap())
    }

    /// STATEMENT: TLS version and cipher queries expose matching negotiated metadata.
    ///
    /// HOW:
    /// - Handshake with default protocols and check TLS 1.3, matching cipher IDs, and IANA names.
    /// - Restrict the client to TLS 1.2 and check its version and both peers' matching cipher.
    #[ntex::test]
    async fn version_and_cipher() {
        let (client, server) = handshake_pair(false, "localhost").await;
        assert_eq!(
            client.query::<TlsVersion>().as_ref(),
            Some(&TlsVersion::Tls13)
        );
        assert_eq!(
            server.query::<TlsVersion>().as_ref(),
            Some(&TlsVersion::Tls13)
        );
        let cipher = *client.query::<TlsCipher>().as_ref().unwrap();
        assert_eq!(server.query::<TlsCipher>().as_ref(), Some(&cipher));
        assert_eq!(cipher.id >> 8, 0x13);
        assert!(cipher.name.unwrap().starts_with("TLS_"));
        assert!(!cipher.name.unwrap().starts_with("TLS13_"));

        // tls 1.2 suite names are the IANA names
        let cfg = ClientConfig::builder_with_provider(
            tls_rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[&tls_rustls::version::TLS12])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
        let (client, server) = pair();
        let (client, server) = join(
            TlsClientFilter::create(
                Io::new(client, SharedCfg::new("CLI")),
                Arc::new(cfg),
                ServerName::try_from("localhost".to_string()).unwrap(),
            ),
            TlsServerFilter::create(
                Io::new(server, SharedCfg::new("SRV")),
                server_config(false),
                Millis(5_000),
            ),
        )
        .await;
        let (client, server) = (client.unwrap(), server.unwrap());
        assert_eq!(
            client.query::<TlsVersion>().as_ref(),
            Some(&TlsVersion::Tls12)
        );
        let cipher = *server.query::<TlsCipher>().as_ref().unwrap();
        assert_eq!(client.query::<TlsCipher>().as_ref(), Some(&cipher));
        assert!(cipher.name.unwrap().starts_with("TLS_ECDHE_"));
    }

    /// STATEMENT: rustls services expose negotiated metadata and carry data through clean shutdown.
    ///
    /// HOW:
    /// - Connect an in-memory client and acceptor with `h2`, checking ALPN, SNI, certificate
    ///   representations, and absent client certificates and unsupported queries.
    /// - Receive 256 KiB on the server, check a reply, then verify shutdown yields peer EOF.
    #[ntex::test]
    async fn acceptor_and_connector() {
        let (client, server) = pair();
        let client = Rc::new(RefCell::new(Some(Io::new(client, SharedCfg::new("CLI")))));

        let acceptor = TlsAcceptor::from(Arc::unwrap_or_clone(server_config(true)));
        assert!(format!("{acceptor:?}").contains("TlsAcceptor"));
        let acceptor = Pipeline::new((), acceptor);

        let cfg = client_config(true);
        let connector = TlsConnector::<Connector<&str>>::from(&cfg)
            .connector(fn_service(async move |_: Connect<&str>| {
                Ok::<_, Error<ConnectError>>(client.borrow_mut().take().unwrap())
            }))
            .clone();
        assert!(format!("{connector:?}").contains("TlsConnector"));
        let connector = Pipeline::new(SharedCfg::new("CLI").build(), connector);

        let (server, client) = join(
            acceptor.call(Io::new(server, SharedCfg::new("SRV"))),
            connector.call(Connect::new("localhost:443")),
        )
        .await;
        let (server, client) = (server.unwrap(), client.unwrap());

        assert_eq!(
            client.query::<HttpProtocol>().as_ref(),
            Some(&HttpProtocol::Http2)
        );
        assert_eq!(
            server.query::<HttpProtocol>().as_ref(),
            Some(&HttpProtocol::Http2)
        );
        assert!(client.query::<PeerCert<'_>>().as_ref().is_some());
        assert_eq!(
            client
                .query::<PeerCertChain<'_>>()
                .as_ref()
                .map(|c| c.0.len()),
            Some(1)
        );
        let cert = client.query::<PeerCert<'_>>().as_ref().unwrap().0.to_vec();
        assert_eq!(
            client
                .query::<crate::PeerCertDer>()
                .into_inner()
                .map(|c| c.0),
            Some(cert.clone().into())
        );
        assert_eq!(
            client
                .query::<crate::PeerCertChainDer>()
                .into_inner()
                .map(|c| c.0),
            Some(vec![cert.into()])
        );
        assert!(client.query::<Servername>().as_ref().is_none());
        assert_eq!(
            server.query::<Servername>().as_ref().map(|s| s.0.as_str()),
            Some("localhost")
        );
        // no client auth
        assert!(server.query::<PeerCert<'_>>().as_ref().is_none());
        assert!(server.query::<PeerCertChain<'_>>().as_ref().is_none());
        assert!(server.query::<crate::PeerCertDer>().as_ref().is_none());
        assert!(server.query::<crate::PeerCertChainDer>().as_ref().is_none());
        assert!(server.query::<u32>().as_ref().is_none());

        // larger than the session buffer limit
        let data = Bytes::from(vec![b'a'; 256 * 1024]);
        client.send(data.clone(), &BytesCodec).await.unwrap();
        let mut received = 0;
        while received < data.len() {
            received += server.recv(&BytesCodec).await.unwrap().unwrap().len();
        }
        server
            .send(Bytes::from_static(b"reply"), &BytesCodec)
            .await
            .unwrap();
        assert_eq!(
            client.recv(&BytesCodec).await.unwrap().unwrap(),
            Bytes::from_static(b"reply")
        );

        // close_notify is exchanged in both directions
        let (res, ()) = join(client.shutdown(), async {
            assert!(server.recv(&BytesCodec).await.unwrap().is_none());
        })
        .await;
        res.unwrap();
    }

    /// STATEMENT: A connection without ALPN defaults to HTTP/1 and an IP host sends no SNI.
    ///
    /// HOW:
    /// - Handshake with ALPN disabled and `127.0.0.1` as the host.
    /// - Check the client's HTTP protocol and the absence of the server's `Servername`.
    #[ntex::test]
    async fn without_alpn_and_sni() {
        let (client, server) = handshake_pair(false, "127.0.0.1").await;
        assert_eq!(
            client.query::<HttpProtocol>().as_ref(),
            Some(&HttpProtocol::Http1)
        );
        // no SNI for ip addresses
        assert!(server.query::<Servername>().as_ref().is_none());
    }

    /// STATEMENT: Shutdown succeeds after a peer disconnects without sending `close_notify`.
    ///
    /// HOW:
    /// - Complete a handshake, drop the server, and check the client receives EOF.
    /// - Await client shutdown and assert it succeeds.
    #[ntex::test]
    async fn shutdown_after_peer_disconnect() {
        let (client, server) = handshake_pair(false, "localhost").await;
        // the peer goes away without close_notify
        drop(server);
        assert!(client.recv(&BytesCodec).await.unwrap().is_none());
        client.shutdown().await.unwrap();
    }

    /// STATEMENT: Peer-close notifications release TLS borrows before waking reentrant shutdown.
    ///
    /// HOW:
    /// - Close each side in turn using a transport with an inline wake hook.
    /// - Check the hook sees no TLS borrow, pending data is delivered in both directions,
    ///   and both transports reach closed status.
    #[ntex::test]
    async fn peer_close_inline_waker() {
        use crate::utils::testing;

        for client_closes in [true, false] {
            let (client, client_ctx) = testing::io("CLI");
            let (server, server_ctx) = testing::io("SRV");
            let (client, server) = testing::handshake(
                join(
                    TlsClientFilter::create(
                        client,
                        client_config(false),
                        ServerName::try_from("localhost").unwrap(),
                    ),
                    TlsServerFilter::create(server, server_config(false), Millis::ZERO),
                ),
                &client_ctx,
                &server_ctx,
            );
            let (client, server) = (client.unwrap(), server.unwrap());
            if client_closes {
                testing::peer_close(&client, &client_ctx, &server, &server_ctx).await;
            } else {
                testing::peer_close(&server, &server_ctx, &client, &client_ctx).await;
            }
        }
    }

    /// STATEMENT: Draining buffered input does not report a transport EOF to rustls.
    ///
    /// HOW:
    /// - Exchange and verify 100-byte and 40,000-byte messages through manually driven transports.
    /// - Read each drained session directly and check for `WouldBlock`, not `UnexpectedEof`.
    #[ntex::test]
    async fn read_keeps_session_open() {
        use crate::utils::testing;

        let (client, client_ctx) = testing::io("CLI");
        let (server, server_ctx) = testing::io("SRV");
        let (client, server) = testing::handshake(
            join(
                TlsClientFilter::create(
                    client,
                    client_config(false),
                    ServerName::try_from("localhost").unwrap(),
                ),
                TlsServerFilter::create(server, server_config(false), Millis::ZERO),
            ),
            &client_ctx,
            &server_ctx,
        );
        let (client, server) = (client.unwrap(), server.unwrap());

        for size in [100, 40_000] {
            let data = Bytes::from(vec![b'x'; size]);
            client.encode(data.clone(), &BytesCodec).unwrap();
            assert!(testing::transfer(&client_ctx, &server_ctx) > 0);
            assert_eq!(server.decode(&BytesCodec).unwrap(), Some(data.clone()));
            server.encode(data.clone(), &BytesCodec).unwrap();
            assert!(testing::transfer(&server_ctx, &client_ctx) > 0);
            assert_eq!(client.decode(&BytesCodec).unwrap(), Some(data));

            // rustls reports a seen EOF as UnexpectedEof instead of WouldBlock
            let srv = server
                .filter()
                .stream(|s| io::Read::read(&mut s.session.reader(), &mut [0]));
            let cli = client
                .filter()
                .stream(|s| io::Read::read(&mut s.session.reader(), &mut [0]));
            assert_eq!(srv.unwrap_err().kind(), io::ErrorKind::WouldBlock);
            assert_eq!(cli.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        }
    }

    /// STATEMENT: TLS 1.3 key-update requests produce responses without application writes.
    ///
    /// HOW:
    /// - Drive an in-memory handshake and request a key update from each peer in turn.
    /// - Transfer each request and check the other filter immediately queues a response.
    /// - Verify bidirectional plaintext after both key changes.
    #[ntex::test]
    async fn key_update_without_application_writes() {
        use ntex_io::FilterLayer;

        use crate::utils::testing;

        let (client, client_ctx) = testing::io("CLI");
        let (server, server_ctx) = testing::io("SRV");
        let (client, server) = testing::handshake(
            join(
                TlsClientFilter::create(
                    client,
                    client_config(false),
                    ServerName::try_from("localhost").unwrap(),
                ),
                TlsServerFilter::create(server, server_config(false), Millis::ZERO),
            ),
            &client_ctx,
            &server_ctx,
        );
        let (client, server) = (client.unwrap(), server.unwrap());

        for client_updates in [true, false] {
            assert_eq!(testing::transfer(&client_ctx, &server_ctx), 0);
            assert_eq!(testing::transfer(&server_ctx, &client_ctx), 0);
            if client_updates {
                client
                    .filter()
                    .stream(|s| s.session.refresh_traffic_keys().unwrap());
                client
                    .with_buf(|buf| client.filter().process_write_buf(buf))
                    .unwrap()
                    .unwrap();
                assert!(testing::transfer(&client_ctx, &server_ctx) > 0);
                assert!(testing::transfer(&server_ctx, &client_ctx) > 0);
            } else {
                server
                    .filter()
                    .stream(|s| s.session.refresh_traffic_keys().unwrap());
                server
                    .with_buf(|buf| server.filter().process_write_buf(buf))
                    .unwrap()
                    .unwrap();
                assert!(testing::transfer(&server_ctx, &client_ctx) > 0);
                assert!(testing::transfer(&client_ctx, &server_ctx) > 0);
            }

            client.encode_slice(b"request after key update").unwrap();
            assert!(testing::transfer(&client_ctx, &server_ctx) > 0);
            assert_eq!(
                server.decode(&BytesCodec).unwrap().unwrap(),
                "request after key update"
            );
            server.encode_slice(b"reply after key update").unwrap();
            assert!(testing::transfer(&server_ctx, &client_ctx) > 0);
            assert_eq!(
                client.decode(&BytesCodec).unwrap().unwrap(),
                "reply after key update"
            );
        }
    }

    /// STATEMENT: An invalid record after negotiation is reported as invalid TLS data.
    ///
    /// HOW:
    /// - Complete a handshake and inject non-TLS bytes through the raw server transport.
    /// - Check a bounded client receive returns `InvalidData` instead of plaintext or EOF.
    #[ntex::test]
    async fn invalid_data_after_handshake() {
        let (client, server) = pair();
        let peer = server.clone();
        let (client, server) = join(
            TlsClientFilter::create(
                Io::new(client, SharedCfg::new("CLI")),
                client_config(false),
                ServerName::try_from("localhost").unwrap(),
            ),
            TlsServerFilter::create(
                Io::new(server, SharedCfg::new("SRV")),
                server_config(false),
                Millis::ZERO,
            ),
        )
        .await;
        let (client, _server) = (client.unwrap(), server.unwrap());
        peer.write(b"garbage garbage garbage");
        let err = ntex_util::time::timeout(Millis(1_000), client.recv(&BytesCodec))
            .await
            .expect("invalid record was not rejected")
            .unwrap_err();
        let ntex_util::future::Either::Right(err) = err else {
            panic!("unexpected codec error: {err:?}");
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// STATEMENT: Connector timeout errors use service configuration rather than I/O configuration.
    ///
    /// HOW:
    /// - Return an idle transport with its TLS timeout disabled from a custom connector.
    /// - Set a 50 ms service timeout and check the bounded call returns `TimedOut` with
    ///   the service's configured diagnostic identity.
    #[ntex::test]
    async fn connector_handshake_timeout() {
        use ntex_error::ErrorDiagnostic;

        let (client, _server) = pair();
        let client = Rc::new(RefCell::new(Some(Io::new(
            client,
            SharedCfg::new("IO")
                .service("transport")
                .add(tls_cfg(Millis::ZERO)),
        ))));
        let connector = TlsConnector::<Connector<&str>>::from(client_config(false)).connector(
            fn_service(async move |_: Connect<&str>| {
                Ok::<_, Error<ConnectError>>(client.borrow_mut().take().unwrap())
            }),
        );
        let connector = Pipeline::new(
            SharedCfg::new("CLI")
                .service("tls-client")
                .add(tls_cfg(Millis(50)))
                .build(),
            connector,
        );
        let err =
            ntex_util::time::timeout(Millis(1_000), connector.call(Connect::new("localhost")))
                .await
                .expect("connector ignored its handshake timeout")
                .unwrap_err();
        assert_eq!(err.service(), Some("tls-client"));
        let ConnectError::Io(err) = &*err else {
            panic!("unexpected connector error: {err:?}");
        };
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    /// STATEMENT: A stalled server handshake fails with the configured timeout.
    ///
    /// HOW:
    /// - Keep the peer open without sending handshake data and configure a 50 ms timeout.
    /// - Call the acceptor and check for `TimedOut` and released handshake capacity.
    #[ntex::test]
    async fn handshake_timeout() {
        let (_client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis(50))));
        let err = Pipeline::new((), TlsAcceptor::new(server_config(false)))
            .call(io)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        MAX_SSL_ACCEPT_COUNTER.with(|c| assert_eq!(c.total(), 0));
    }

    /// STATEMENT: Peer disconnects during handshakes propagate through server and connector errors.
    ///
    /// HOW:
    /// - Close the client during server creation and check for `UnexpectedEof`.
    /// - Close the server during a connector call and check the call returns an error.
    #[ntex::test]
    async fn handshake_disconnect() {
        let (client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV"));
        let (res, ()) = join(
            TlsServerFilter::create(io, server_config(false), Millis::ZERO),
            client.close(),
        )
        .await;
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);

        // client side, the error is reported by the connector
        let (client, server) = pair();
        let client = Rc::new(RefCell::new(Some(Io::new(client, SharedCfg::new("CLI")))));
        let connector =
            TlsConnector::<Connector<&str>>::new(Arc::unwrap_or_clone(client_config(false)))
                .connector(fn_service(async move |_: Connect<&str>| {
                    Ok::<_, Error<ConnectError>>(client.borrow_mut().take().unwrap())
                }));
        let (res, ()) = join(
            Pipeline::new(SharedCfg::new("CLI").build(), connector).call(Connect::new("localhost")),
            server.close(),
        )
        .await;
        assert!(res.is_err());
    }

    /// STATEMENT: Plain HTTP input is rejected as invalid TLS handshake data.
    ///
    /// HOW:
    /// - Write an HTTP request into an in-memory server transport.
    /// - Attempt the TLS handshake and check for `InvalidData`.
    #[ntex::test]
    async fn handshake_invalid_data() {
        let (client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV"));
        client.write(b"GET / HTTP/1.1\r\n\r\n");
        let err = TlsServerFilter::create(io, server_config(false), Millis::ZERO)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// STATEMENT: Acceptor readiness waits for handshake capacity and resumes when it is released.
    ///
    /// HOW:
    /// - Set capacity to one and start a stalled handshake through a bound acceptor.
    /// - Check readiness becomes pending, close the peer, then check failure restores readiness.
    /// - Poll another stalled call, drop it, and check cancellation releases its slot and readiness.
    #[ntex::test]
    async fn acceptor_waits_for_capacity() {
        use std::future::Future;

        MAX_SSL_ACCEPT_COUNTER.with(|c| c.set_capacity(1));
        let acceptor = Pipeline::new((), TlsAcceptor::new(server_config(false)));

        let (client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis(30_000))));
        let acceptor2 = acceptor.bind();
        let hnd = ntex::rt::spawn(async move { acceptor2.call(io).await });

        // wait until the handshake holds the only slot
        let mut n = 0;
        while lazy(|cx| acceptor.poll_ready(cx)).await.is_ready() {
            n += 1;
            assert!(n < 1000, "handshake did not start");
            sleep(Millis(1)).await;
        }
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_pending());

        // capacity is released by the failed handshake
        client.close().await;
        assert!(hnd.await.unwrap().is_err());
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_ready());

        let (_client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis::ZERO)));
        let mut call = Box::pin(acceptor.call(io));
        assert!(lazy(|cx| call.as_mut().poll(cx)).await.is_pending());
        MAX_SSL_ACCEPT_COUNTER.with(|c| assert_eq!(c.total(), 1));
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_pending());
        drop(call);
        MAX_SSL_ACCEPT_COUNTER.with(|c| assert_eq!(c.total(), 0));
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_ready());
        MAX_SSL_ACCEPT_COUNTER.with(|c| c.set_capacity(256));
    }

    /// STATEMENT: Queued pages are batched into TLS records without losing partial writes.
    ///
    /// HOW:
    /// - Queue varied page layouts and tails, then verify a zero session limit preserves input.
    /// - Enable bounded writes, check the queue drains with the expected record count,
    ///   and compare the decrypted bytes with all original pages and their tail.
    #[test]
    #[allow(clippy::assert_is_empty)]
    fn write_gathers_pages_into_records() {
        use std::io::Read;

        use ntex_bytes::{BytePageSize, BytePages};
        use tls_rustls::{ClientConnection, ServerConnection};

        // moves tls records between sessions, returns the records and the
        // received plaintext
        macro_rules! transfer {
            ($from:expr, $to:expr) => {{
                let mut data = Vec::new();
                while $from.wants_write() {
                    $from.write_tls(&mut data).unwrap();
                }
                let mut plain = Vec::new();
                let mut src = &data[..];
                while !src.is_empty() {
                    $to.read_tls(&mut src).unwrap();
                    $to.process_new_packets().unwrap();
                    let _ = $to.reader().read_to_end(&mut plain);
                }
                (data, plain)
            }};
        }
        let limit = BytePageSize::Size16.capacity();
        let mut client = ClientConnection::new(
            client_config(false),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        client.set_buffer_limit(Some(limit));
        let mut server = ServerConnection::new(server_config(false)).unwrap();
        while client.is_handshaking() || server.is_handshaking() {
            transfer!(client, server);
            transfer!(server, client);
        }

        for (sizes, tail, buffer_limit, expected) in [
            (&[300, 8192][..], 0, limit, 1),
            (&[300, limit - 300][..], 0, limit, 1),
            (&[300, 16384][..], 0, limit, 2),
            (&[100, 5000, 6000, 7000][..], 0, limit, 2),
            (&[20000, 300][..], 0, limit, 2),
            (&[20000][..], 0, limit, 2),
            (&[128; 32][..], 0, limit, 1),
            (&[128; 65][..], 0, limit, 3),
            (&[128; 31][..], 128, limit, 1),
            (&[3, 400, 29, 1000, 700][..], 777, limit, 1),
            (&[128; 3][..], 91, 100, 5),
        ] {
            let parts: Vec<_> = (0u8..)
                .zip(sizes)
                .map(|(i, &n)| Bytes::from(vec![i; n]))
                .collect();
            let mut src = BytePages::new(BytePageSize::Size16);
            for p in &parts {
                src.append(p.clone());
            }
            assert_eq!(src.num_pages(), parts.len());
            let tail = vec![255; tail];
            src.extend_from_slice(&tail);

            let mut dst = BytePages::new(BytePageSize::Size16);
            client.set_buffer_limit(Some(0));
            stream::write_buf(&mut client, &mut src, &mut dst).unwrap();
            let expected_plain: Vec<u8> = parts
                .iter()
                .flat_map(|p| p.iter().copied())
                .chain(tail.iter().copied())
                .collect();
            assert!(dst.is_empty());
            assert_eq!(
                src.iter().flatten().copied().collect::<Vec<_>>(),
                expected_plain,
                "zero progress must retain the input"
            );

            client.set_buffer_limit(Some(buffer_limit));
            stream::write_buf(&mut client, &mut src, &mut dst).unwrap();
            assert!(src.is_empty() && !client.wants_write());

            let wire = dst.freeze();
            let mut records = 0;
            let mut rest = &wire[..];
            while rest.len() >= 5 {
                rest = &rest[5 + usize::from(u16::from_be_bytes([rest[3], rest[4]]))..];
                records += 1;
            }
            assert!(rest.is_empty());
            assert_eq!(records, expected, "{sizes:?}");

            let mut plain = Vec::new();
            let mut rd = &wire[..];
            while !rd.is_empty() {
                server.read_tls(&mut rd).unwrap();
                server.process_new_packets().unwrap();
                let _ = server.reader().read_to_end(&mut plain);
            }
            assert_eq!(plain, expected_plain, "{sizes:?}");
        }
    }
}
