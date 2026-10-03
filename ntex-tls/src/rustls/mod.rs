//! An implementation of TLS streams for ntex backed by rustls
use tls_rustls::pki_types::CertificateDer;

mod accept;
mod client;
mod connect;
mod server;
mod stream;

pub use self::accept::TlsAcceptor;
pub use self::client::TlsClientFilter;
pub use self::connect::TlsConnector;
pub use self::server::TlsServerFilter;

use self::stream::Stream;

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert<'a>(pub CertificateDer<'a>);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain<'a>(pub Vec<CertificateDer<'a>>);

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
    use crate::{MAX_SSL_ACCEPT_COUNTER, Servername, TlsConfig};

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
            client.query::<crate::PeerCertDer>().as_ref().map(|c| &c.0),
            Some(&cert)
        );
        assert_eq!(
            client
                .query::<crate::PeerCertChainDer>()
                .as_ref()
                .map(|c| &c.0),
            Some(&vec![cert])
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

    #[ntex::test]
    async fn shutdown_after_peer_disconnect() {
        let (client, server) = handshake_pair(false, "localhost").await;
        // the peer goes away without close_notify
        drop(server);
        assert!(client.recv(&BytesCodec).await.unwrap().is_none());
        client.shutdown().await.unwrap();
    }

    #[ntex::test]
    async fn handshake_timeout() {
        let (_client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis(50))));
        let err = Pipeline::new((), TlsAcceptor::new(server_config(false)))
            .call(io)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

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

    #[ntex::test]
    async fn acceptor_waits_for_capacity() {
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
        MAX_SSL_ACCEPT_COUNTER.with(|c| c.set_capacity(256));
    }

    /// Two consecutive pages are written into one record when they fit.
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

        for (sizes, expected) in [
            (&[300, 8192][..], 1),
            (&[300, limit - 300][..], 1),
            (&[300, 16384][..], 2),
            (&[100, 5000, 6000, 7000][..], 2),
            (&[20000, 300][..], 2),
            (&[20000][..], 2),
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

            let mut dst = BytePages::new(BytePageSize::Size16);
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
            let expected_plain: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
            assert_eq!(plain, expected_plain, "{sizes:?}");
        }
    }
}
