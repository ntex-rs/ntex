//! An implementation of TLS streams for ntex backed by rustls
use std::{future::Future, io};

use ntex_io::Io;
use ntex_util::time::{Millis, timeout_checked};
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

/// Drive the handshake until the session stops handshaking.
///
/// `state` reports the session's `(wants_write, is_handshaking)` flags.
async fn handshake<F>(io: &Io<F>, state: impl Fn() -> (bool, bool)) -> io::Result<()> {
    let mut eof = false;
    loop {
        let (wants_write, handshaking) = state();
        if wants_write {
            io.flush(false).await?;
        }
        if !handshaking {
            return Ok(());
        }
        if eof {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "disconnected"));
        }
        // The read that reports eof may also carry the peer's last handshake
        // flight, so the handshake state is checked once more before the eof
        // is treated as a failure.
        eof = io.read_notify().await?.is_none();
    }
}

/// Run handshake with timeout, zero timeout disables it
async fn with_timeout<R>(
    timeout: Millis,
    fut: impl Future<Output = io::Result<R>>,
) -> io::Result<R> {
    timeout_checked(timeout, fut).await.unwrap_or_else(|()| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "TLS Handshake timeout",
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    use ntex::codec::BytesCodec;
    use ntex_bytes::Bytes;
    use ntex_error::Error;
    use ntex_io::{Layer, testing::IoTest, types::HttpProtocol};
    use ntex_net::connect::{Connect, ConnectError, Connector};
    use ntex_service::{Pipeline, cfg::SharedCfg, fn_service};
    use ntex_util::{future::join, future::lazy, time::sleep};
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
        assert!(client.query::<Servername>().as_ref().is_none());
        assert_eq!(
            server.query::<Servername>().as_ref().map(|s| s.0.as_str()),
            Some("localhost")
        );
        // no client auth
        assert!(server.query::<PeerCert<'_>>().as_ref().is_none());
        assert!(server.query::<PeerCertChain<'_>>().as_ref().is_none());
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
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::NotConnected);

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
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis(100))));
        let acceptor2 = acceptor.bind();
        let hnd = ntex::rt::spawn(async move { acceptor2.call(io).await });
        sleep(Millis(10)).await;
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_pending());

        // capacity is released by the timed out handshake
        assert!(hnd.await.unwrap().is_err());
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_ready());
        drop(client);
        MAX_SSL_ACCEPT_COUNTER.with(|c| c.set_capacity(256));
    }
}
