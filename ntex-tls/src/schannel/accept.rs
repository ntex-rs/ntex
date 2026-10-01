use std::{fmt, io};

use ntex_io::{Filter, Io, Layer};
use ntex_service::{Ctx, Service, cfg::Cfg, cfg::Configuration};
use ntex_util::{services::Counter, time::timeout_checked};

use super::{SchannelFilter, ServerConfig, accept as accept_io};
use crate::{MAX_SSL_ACCEPT_COUNTER, TlsConfig};

#[derive(Clone)]
/// Support `TLS` server connections via Windows Schannel
///
/// `schannel` feature enables `TlsAcceptor` type
pub struct TlsAcceptor {
    config: ServerConfig,
    conns: Counter,
}

impl TlsAcceptor {
    /// Create Schannel acceptor service
    pub fn new(config: ServerConfig) -> Self {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| TlsAcceptor {
            config,
            conns: conns.clone(),
        })
    }
}

impl From<ServerConfig> for TlsAcceptor {
    fn from(config: ServerConfig) -> Self {
        Self::new(config)
    }
}

impl<F: Filter, St> Service<St, Io<F>> for TlsAcceptor {
    type Res = Io<Layer<SchannelFilter, F>>;
    type Error = io::Error;

    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        if !self.conns.is_available() {
            self.conns.available().await;
        }
        Ok(())
    }

    async fn call(&self, io: Io<F>, _: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        let _guard = self.conns.get();
        let cfg: Cfg<TlsConfig> = io.cfg().ctx().get();

        log::trace!("{}: Accepting tls connection", io.tag());
        timeout_checked(cfg.handshake_timeout(), accept_io(io, &self.config))
            .await
            .unwrap_or_else(|()| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "TLS Handshake timeout",
                ))
            })
    }
}

impl fmt::Debug for TlsAcceptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsAcceptor").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Read, io::Write, sync::Arc};

    use ntex::codec::BytesCodec;
    use ntex_bytes::Bytes;
    use ntex_io::{testing::IoTest, types::HttpProtocol};
    use ntex_service::{Pipeline, cfg::SharedCfg};
    use ntex_util::{future::join, future::lazy, time::Millis, time::sleep, time::timeout};
    use tls_rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tls_rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tls_rustls::{DigitallySignedStruct, SignatureScheme};

    use super::*;
    use crate::Servername;
    use crate::schannel::{Certificate, ClientConfig, PeerCert, connect};

    const PFX: &[u8] = include_bytes!("../../examples/identity.pfx");

    fn identity() -> Certificate {
        Certificate::from_pkcs12(PFX, "ntex").unwrap()
    }

    fn client_config() -> ClientConfig {
        ClientConfig::new().danger_accept_invalid_certs(true)
    }

    type SchannelIo = Io<Layer<SchannelFilter>>;

    async fn handshake_pair(
        server: &ServerConfig,
        client: ClientConfig,
        host: &str,
    ) -> (io::Result<SchannelIo>, io::Result<SchannelIo>) {
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        join(
            connect(Io::new(cli, SharedCfg::new("CLI")), host, client),
            accept_io(Io::new(srv, SharedCfg::new("SRV")), server),
        )
        .await
    }

    async fn connected(
        server: &ServerConfig,
        client: ClientConfig,
        host: &str,
    ) -> (SchannelIo, SchannelIo) {
        let (client, server) = handshake_pair(server, client, host).await;
        (client.unwrap(), server.unwrap())
    }

    fn protocol(io: &SchannelIo) -> Option<HttpProtocol> {
        io.query::<HttpProtocol>().as_ref().copied()
    }

    #[ntex::test]
    async fn test_accept() {
        let config = ServerConfig::new(identity()).unwrap();
        let (client, server) = connected(&config, client_config(), "localhost").await;

        assert_eq!(protocol(&client), Some(HttpProtocol::Http2));
        assert_eq!(protocol(&server), Some(HttpProtocol::Http2));
        assert_eq!(
            server.query::<Servername>().as_ref().map(|s| s.0.as_str()),
            Some("localhost")
        );
        assert!(client.query::<Servername>().as_ref().is_none());
        // no client certificate requested
        assert!(server.query::<PeerCert>().as_ref().is_none());
        let server_cert = client.query::<PeerCert>().as_ref().map(|c| c.0.clone());
        assert_eq!(server_cert.as_deref(), Some(identity().der()));

        // larger than a TLS record
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
    async fn test_accept_alpn() {
        let cert = identity();

        // the server's preference wins
        let config = ServerConfig::new(cert.clone())
            .unwrap()
            .set_alpn_protocols(&["http/1.1", "h2"]);
        let (client, server) = connected(&config, client_config(), "localhost").await;
        assert_eq!(protocol(&client), Some(HttpProtocol::Http1));
        assert_eq!(protocol(&server), Some(HttpProtocol::Http1));

        let config = ServerConfig::new(cert.clone())
            .unwrap()
            .set_alpn_protocols(&["h2"]);
        let client_cfg = client_config().set_alpn_protocols(&["http/1.1", "h2"]);
        let (client, server) = connected(&config, client_cfg, "localhost").await;
        assert_eq!(protocol(&client), Some(HttpProtocol::Http2));
        assert_eq!(protocol(&server), Some(HttpProtocol::Http2));

        // ALPN disabled, no SNI for ip addresses
        let config = ServerConfig::new(cert)
            .unwrap()
            .set_alpn_protocols::<&str>(&[]);
        let (client, server) = connected(&config, client_config(), "127.0.0.1").await;
        assert_eq!(server.filter().inner().ctx.alpn_protocol(), None);
        assert_eq!(protocol(&client), Some(HttpProtocol::Http1));
        assert!(server.query::<Servername>().as_ref().is_none());
    }

    /// The negotiated protocol is kept after post-handshake messages.
    #[ntex::test]
    async fn test_accept_alpn_after_session_ticket() {
        let config = ServerConfig::new(identity()).unwrap();
        let (client, server) = connected(&config, client_config(), "localhost").await;

        // TLS 1.3 session tickets precede the data
        server
            .send(Bytes::from_static(b"hello"), &BytesCodec)
            .await
            .unwrap();
        assert_eq!(client.recv(&BytesCodec).await.unwrap().unwrap(), "hello");
        client
            .send(Bytes::from_static(b"reply"), &BytesCodec)
            .await
            .unwrap();
        assert_eq!(server.recv(&BytesCodec).await.unwrap().unwrap(), "reply");

        assert_eq!(protocol(&client), Some(HttpProtocol::Http2));
        assert_eq!(protocol(&server), Some(HttpProtocol::Http2));
    }

    #[ntex::test]
    async fn test_accept_client_cert() {
        let cert = identity();
        let config = ServerConfig::new(cert.clone())
            .unwrap()
            .request_client_cert(true);

        let client_cfg = client_config().set_client_cert(cert.clone());
        let (client, server) = connected(&config, client_cfg, "localhost").await;
        let peer = server.query::<PeerCert>().as_ref().map(|c| c.0.clone());
        assert_eq!(peer.as_deref(), Some(cert.der()));
        client
            .send(Bytes::from_static(b"hello"), &BytesCodec)
            .await
            .unwrap();
        assert_eq!(server.recv(&BytesCodec).await.unwrap().unwrap(), "hello");

        // the certificate is optional
        let (client, server) = connected(&config, client_config(), "localhost").await;
        assert!(server.query::<PeerCert>().as_ref().is_none());
        client
            .send(Bytes::from_static(b"hello"), &BytesCodec)
            .await
            .unwrap();
        assert_eq!(server.recv(&BytesCodec).await.unwrap().unwrap(), "hello");

        // not sent unless requested
        let config = ServerConfig::new(cert.clone()).unwrap();
        let client_cfg = client_config().set_client_cert(cert);
        let (_client, server) = connected(&config, client_cfg, "localhost").await;
        assert!(server.query::<PeerCert>().as_ref().is_none());
    }

    #[ntex::test]
    async fn test_accept_handshake_failure() {
        let config = ServerConfig::new(identity()).unwrap();

        // the client does not trust the self-signed certificate
        let (client, server) = handshake_pair(&config, ClientConfig::new(), "localhost").await;
        assert!(client.is_err());
        assert!(server.is_err());

        // not a TLS client
        let (cli, srv) = IoTest::create();
        cli.write("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
        let err = accept_io(Io::new(srv, SharedCfg::new("SRV")), &config)
            .await
            .unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");

        // the client disconnects during the handshake
        let (cli, srv) = IoTest::create();
        cli.close().await;
        let err = accept_io(Io::new(srv, SharedCfg::new("SRV")), &config)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    fn rustls_client() -> (tls_rustls::ClientConnection, Vec<u8>) {
        let mut cfg = rustls_config(&tls_rustls::version::TLS13);
        cfg.alpn_protocols = vec![b"h2".to_vec()];
        let mut conn = tls_rustls::ClientConnection::new(
            Arc::new(cfg),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut hello = Vec::new();
        conn.write_tls(&mut hello).unwrap();
        (conn, hello)
    }

    /// Completes the handshake of a rustls client after its `ClientHello`.
    async fn rustls_handshake(cli: &IoTest, conn: &mut tls_rustls::ClientConnection) {
        while conn.is_handshaking() {
            let data = cli.read().await.unwrap();
            assert!(!data.is_empty(), "the server closed the connection");
            conn.read_tls(&mut &data[..]).unwrap();
            conn.process_new_packets().unwrap();
            let mut out = Vec::new();
            while conn.wants_write() {
                conn.write_tls(&mut out).unwrap();
            }
            cli.write(out);
        }
    }

    async fn check_rustls_session(
        cli: &IoTest,
        mut conn: tls_rustls::ClientConnection,
        server: &SchannelIo,
    ) {
        assert_eq!(protocol(server), Some(HttpProtocol::Http2));
        assert_eq!(
            server.query::<Servername>().as_ref().map(|s| s.0.as_str()),
            Some("localhost")
        );
        conn.writer().write_all(b"hello").unwrap();
        let mut out = Vec::new();
        conn.write_tls(&mut out).unwrap();
        cli.write(out);
        assert_eq!(server.recv(&BytesCodec).await.unwrap().unwrap(), "hello");
    }

    /// The `ClientHello` arrives in pieces, Schannel creates the context
    /// once it is complete.
    #[ntex::test]
    async fn test_accept_split_client_hello() {
        let config = ServerConfig::new(identity()).unwrap();
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let (mut conn, hello) = rustls_client();

        let client = async {
            for piece in [&hello[..3], &hello[3..10], &hello[10..]] {
                cli.write(piece);
                sleep(Millis(10)).await;
            }
            rustls_handshake(&cli, &mut conn).await;
        };
        let server = accept_io(Io::new(srv, SharedCfg::new("SRV")), &config);
        let (server, ()) = timeout(Millis(5_000), join(server, client)).await.unwrap();
        check_rustls_session(&cli, conn, &server.unwrap()).await;
    }

    /// The `ClientHello` is buffered before the handshake starts.
    #[ntex::test]
    async fn test_accept_buffered_client_hello() {
        let config = ServerConfig::new(identity()).unwrap();
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let (mut conn, hello) = rustls_client();

        cli.write(&hello);
        let io = Io::new(srv, SharedCfg::new("SRV"));
        // the hello is read into the buffer
        assert!(io.read_notify().await.unwrap().is_some());

        let server = accept_io(io, &config);
        let (server, ()) = timeout(
            Millis(5_000),
            join(server, rustls_handshake(&cli, &mut conn)),
        )
        .await
        .unwrap();
        check_rustls_session(&cli, conn, &server.unwrap()).await;
    }

    /// The client updates its traffic keys after the handshake.
    #[ntex::test]
    async fn test_accept_key_update() {
        let config = ServerConfig::new(identity()).unwrap();
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let (mut conn, hello) = rustls_client();
        cli.write(&hello);

        let server = accept_io(Io::new(srv, SharedCfg::new("SRV")), &config);
        let (server, ()) = timeout(
            Millis(5_000),
            join(server, rustls_handshake(&cli, &mut conn)),
        )
        .await
        .unwrap();
        let server = server.unwrap();

        for round in 0..3 {
            conn.refresh_traffic_keys().unwrap();
            let msg = format!("hello {round}");
            conn.writer().write_all(msg.as_bytes()).unwrap();
            let mut out = Vec::new();
            while conn.wants_write() {
                conn.write_tls(&mut out).unwrap();
            }
            cli.write(out);
            let received = timeout(Millis(5_000), server.recv(&BytesCodec))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(received, msg);

            server
                .send(Bytes::from(format!("reply {round}")), &BytesCodec)
                .await
                .unwrap();
            let mut reply = Vec::new();
            while reply.is_empty() {
                let data = timeout(Millis(5_000), cli.read()).await.unwrap().unwrap();
                assert!(!data.is_empty(), "the server closed the connection");
                conn.read_tls(&mut &data[..]).unwrap();
                conn.process_new_packets().unwrap();
                let _ = conn.reader().read_to_end(&mut reply);
            }
            assert_eq!(reply, format!("reply {round}").as_bytes());
        }
    }

    #[ntex::test]
    async fn test_acceptor_handshake_timeout() {
        let (_cli, srv) = IoTest::create();
        let tls_cfg = TlsConfig {
            handshake_timeout: Millis(50),
            ..TlsConfig::default()
        };
        let io = Io::new(srv, SharedCfg::new("SRV").add(tls_cfg));
        let acceptor = TlsAcceptor::from(ServerConfig::new(identity()).unwrap());
        assert!(format!("{acceptor:?}").contains("TlsAcceptor"));
        let err = Pipeline::new((), acceptor).call(io).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[ntex::test]
    async fn test_acceptor_waits_for_capacity() {
        MAX_SSL_ACCEPT_COUNTER.with(|conns| conns.set_capacity(1));
        let acceptor = Pipeline::new((), TlsAcceptor::new(ServerConfig::new(identity()).unwrap()));

        let (cli, srv) = IoTest::create();
        let io = Io::new(srv, SharedCfg::new("SRV"));
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
        cli.close().await;
        assert!(hnd.await.unwrap().is_err());
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_ready());
        MAX_SSL_ACCEPT_COUNTER.with(|conns| conns.set_capacity(256));
    }

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

    fn rustls_config(
        version: &'static tls_rustls::SupportedProtocolVersion,
    ) -> tls_rustls::ClientConfig {
        tls_rustls::ClientConfig::builder_with_provider(
            tls_rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[version])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth()
    }

    /// A rustls client talks to the acceptor service over TCP.
    #[ntex::test]
    async fn test_acceptor_rustls_client() {
        let config = ServerConfig::new(identity()).unwrap();
        let srv = ntex::server::test_server(async move || {
            ntex::service(TlsAcceptor::new(config.clone())).and_then(async |io: SchannelIo| {
                let proto = io.query::<HttpProtocol>().as_ref().copied();
                let name = io.query::<Servername>().as_ref().map(|s| s.0.clone());
                let reply = format!("{proto:?} {name:?}");
                let msg = io.recv(&BytesCodec).await.unwrap().unwrap();
                io.send(msg, &BytesCodec).await.unwrap();
                io.send(Bytes::from(reply), &BytesCodec).await.unwrap();
                io.shutdown().await.unwrap();
                Ok::<_, io::Error>(())
            })
        });
        let addr = srv.addr();

        for version in [&tls_rustls::version::TLS12, &tls_rustls::version::TLS13] {
            let reply = std::thread::spawn(move || {
                let mut cfg = rustls_config(version);
                cfg.alpn_protocols = vec![b"h2".to_vec()];
                let conn = tls_rustls::ClientConnection::new(
                    Arc::new(cfg),
                    ServerName::try_from("localhost").unwrap(),
                )
                .unwrap();
                let sock = std::net::TcpStream::connect(addr).unwrap();
                sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))
                    .unwrap();
                let mut stream = tls_rustls::StreamOwned::new(conn, sock);
                stream.write_all(b"hello").unwrap();
                let mut reply = Vec::new();
                // the server closes with close_notify
                stream.read_to_end(&mut reply).unwrap();
                (stream.conn.protocol_version(), reply)
            })
            .join()
            .unwrap();
            assert_eq!(reply.0, Some(version.version));
            assert_eq!(
                String::from_utf8(reply.1).unwrap(),
                "helloSome(Http2) Some(\"localhost\")"
            );
        }
    }

    /// How the client deviates from the protocol.
    #[derive(Clone, Copy, Debug)]
    enum Tamper {
        None,
        /// Sends a prefix of a flight and closes the connection.
        Truncate {
            flight: usize,
            len: usize,
        },
        /// Flips a byte of a flight and closes the connection.
        Flip {
            flight: usize,
            pos: usize,
        },
        /// Flips a byte of a flight and keeps the connection open.
        FlipOpen {
            flight: usize,
            pos: usize,
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ClientEnd {
        /// The client completed the handshake.
        Done,
        /// The client closed the connection.
        Closed,
        /// The server closed the connection.
        ServerClosed,
        /// The server responded to the tampered flight.
        Responded,
        /// The server neither responded nor closed the connection.
        Waiting,
        /// The client rejected the server's flight and sent an alert.
        Rejected,
    }

    /// Writes `data` in `chunk` sized pieces, the server reads each piece separately.
    async fn deliver(cli: &IoTest, data: &[u8], chunk: usize) {
        for piece in data.chunks(chunk.max(1)) {
            cli.write(piece);
            for _ in 0..100 {
                if cli.remote_buffer(|buf| buf.is_empty()) {
                    break;
                }
                ntex_util::task::yield_to().await;
            }
        }
    }

    fn flight(conn: &mut tls_rustls::ClientConnection) -> Vec<u8> {
        let mut out = Vec::new();
        while conn.wants_write() {
            conn.write_tls(&mut out).unwrap();
        }
        out
    }

    /// Drives a rustls client against `accept()`, the server must complete
    /// with success or an error before the deadline.
    ///
    /// Returns the server result, the client's end and the client's flights.
    async fn negotiate(
        config: &ServerConfig,
        cfg: tls_rustls::ClientConfig,
        chunk: usize,
        tamper: Tamper,
    ) -> (io::Result<SchannelIo>, ClientEnd, Vec<Vec<u8>>) {
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let mut conn = tls_rustls::ClientConnection::new(
            Arc::new(cfg),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();

        let client = async {
            let mut flights = Vec::new();
            loop {
                let idx = flights.len();
                let mut data = flight(&mut conn);
                flights.push(data.clone());
                match tamper {
                    Tamper::Truncate { flight, len } if flight == idx => {
                        deliver(&cli, &data[..len], chunk).await;
                        cli.close().await;
                        return (ClientEnd::Closed, flights);
                    }
                    Tamper::Flip { flight, pos } if flight == idx => {
                        data[pos] ^= 0xff;
                        deliver(&cli, &data, chunk).await;
                        cli.close().await;
                        return (ClientEnd::Closed, flights);
                    }
                    Tamper::FlipOpen { flight, pos } if flight == idx => {
                        data[pos] ^= 0xff;
                        deliver(&cli, &data, chunk).await;
                        let end = match timeout(Millis(500), cli.read()).await {
                            Ok(Ok(data)) if !data.is_empty() => ClientEnd::Responded,
                            Ok(_) => return (ClientEnd::ServerClosed, flights),
                            Err(()) => ClientEnd::Waiting,
                        };
                        // the server must complete once the client is gone
                        cli.close().await;
                        return (end, flights);
                    }
                    _ => deliver(&cli, &data, chunk).await,
                }
                if !conn.is_handshaking() {
                    return (ClientEnd::Done, flights);
                }

                // the server's next flight
                loop {
                    let data = cli.read().await.unwrap();
                    if data.is_empty() {
                        return (ClientEnd::ServerClosed, flights);
                    }
                    let mut rd = &data[..];
                    while !rd.is_empty() {
                        conn.read_tls(&mut rd).unwrap();
                        if conn.process_new_packets().is_err() {
                            // the alert, the connection stays open
                            deliver(&cli, &flight(&mut conn), chunk).await;
                            return (ClientEnd::Rejected, flights);
                        }
                    }
                    if conn.wants_write() || !conn.is_handshaking() {
                        break;
                    }
                }
            }
        };
        let server = accept_io(Io::new(srv, SharedCfg::new("SRV")), config);
        let (server, (end, flights)) = timeout(Millis(10_000), join(server, client))
            .await
            .unwrap_or_else(|()| panic!("negotiation stalls, {tamper:?}"));
        (server, end, flights)
    }

    fn tls_versions() -> [&'static tls_rustls::SupportedProtocolVersion; 2] {
        [&tls_rustls::version::TLS12, &tls_rustls::version::TLS13]
    }

    fn h2_config(
        version: &'static tls_rustls::SupportedProtocolVersion,
    ) -> tls_rustls::ClientConfig {
        let mut cfg = rustls_config(version);
        cfg.alpn_protocols = vec![b"h2".to_vec()];
        cfg
    }

    fn assert_negotiated(server: &SchannelIo) {
        assert_eq!(protocol(server), Some(HttpProtocol::Http2));
        assert_eq!(
            server.query::<Servername>().as_ref().map(|s| s.0.as_str()),
            Some("localhost")
        );
    }

    /// Every flight arrives in pieces of any size.
    #[ntex::test]
    async fn test_negotiate_chunked() {
        for request_cert in [false, true] {
            let config = ServerConfig::new(identity())
                .unwrap()
                .request_client_cert(request_cert);
            for version in tls_versions() {
                for chunk in [1, 2, 3, 5, 7, 16, 100, usize::MAX] {
                    let (server, end, _) =
                        negotiate(&config, h2_config(version), chunk, Tamper::None).await;
                    let server = server.unwrap_or_else(|err| {
                        panic!("{version:?} chunk {chunk} cert {request_cert}: {err}")
                    });
                    assert_eq!(end, ClientEnd::Done);
                    assert_negotiated(&server);
                }
            }
        }
    }

    /// Handshake messages span several TLS records.
    #[ntex::test]
    async fn test_negotiate_fragmented_records() {
        let config = ServerConfig::new(identity()).unwrap();
        for version in tls_versions() {
            for chunk in [1, 13, usize::MAX] {
                let mut cfg = h2_config(version);
                cfg.max_fragment_size = Some(64);
                let (server, end, _) = negotiate(&config, cfg, chunk, Tamper::None).await;
                let server =
                    server.unwrap_or_else(|err| panic!("{version:?} chunk {chunk}: {err}"));
                assert_eq!(end, ClientEnd::Done);
                assert_negotiated(&server);
            }
        }
    }

    /// The client's key share is not the server's preferred group.
    #[ntex::test]
    async fn test_negotiate_key_share() {
        use tls_rustls::crypto::ring::{default_provider, kx_group};

        let config = ServerConfig::new(identity()).unwrap();
        for groups in [
            vec![kx_group::SECP384R1, kx_group::X25519, kx_group::SECP256R1],
            vec![kx_group::SECP256R1, kx_group::X25519],
        ] {
            let provider = tls_rustls::crypto::CryptoProvider {
                kx_groups: groups,
                ..default_provider()
            };
            let mut cfg = tls_rustls::ClientConfig::builder_with_provider(provider.into())
                .with_protocol_versions(&[&tls_rustls::version::TLS13])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth();
            cfg.alpn_protocols = vec![b"h2".to_vec()];
            for chunk in [1, usize::MAX] {
                let (server, end, _) = negotiate(&config, cfg.clone(), chunk, Tamper::None).await;
                assert_negotiated(&server.unwrap());
                assert_eq!(end, ClientEnd::Done);
            }
        }
    }

    /// The client rejects the server's certificate and sends an alert.
    #[ntex::test]
    async fn test_negotiate_client_alert() {
        let config = ServerConfig::new(identity()).unwrap();
        for version in tls_versions() {
            let cfg = tls_rustls::ClientConfig::builder_with_provider(
                tls_rustls::crypto::ring::default_provider().into(),
            )
            .with_protocol_versions(&[version])
            .unwrap()
            .with_root_certificates(tls_rustls::RootCertStore::empty())
            .with_no_client_auth();
            let (server, end, _) = negotiate(&config, cfg, usize::MAX, Tamper::None).await;
            assert_eq!(end, ClientEnd::Rejected, "{version:?}");
            assert!(server.is_err(), "{version:?}");
        }
    }

    /// Runs `cases` concurrently in batches.
    async fn run_cases(
        config: &ServerConfig,
        version: &'static tls_rustls::SupportedProtocolVersion,
        cases: Vec<Tamper>,
    ) -> Vec<(Tamper, io::Result<SchannelIo>, ClientEnd)> {
        let mut results = Vec::new();
        for batch in cases.chunks(16) {
            let res = ntex_util::future::join_all(batch.iter().map(async |tamper| {
                let (server, end, _) =
                    negotiate(config, h2_config(version), usize::MAX, *tamper).await;
                (*tamper, server, end)
            }))
            .await;
            results.extend(res);
        }
        results
    }

    /// The client flights of a successful handshake.
    async fn client_flights(
        config: &ServerConfig,
        version: &'static tls_rustls::SupportedProtocolVersion,
    ) -> Vec<Vec<u8>> {
        let (server, end, mut flights) =
            negotiate(config, h2_config(version), usize::MAX, Tamper::None).await;
        server.unwrap();
        assert_eq!(end, ClientEnd::Done);
        flights.retain(|data| !data.is_empty());
        flights
    }

    /// Positions of record lengths and plaintext handshake message lengths,
    /// increasing one makes the peer wait for more data.
    fn length_fields(data: &[u8]) -> Vec<usize> {
        let mut fields = Vec::new();
        let mut off = 0;
        while off + 5 <= data.len() {
            fields.extend([off + 3, off + 4]);
            if data[off] == 22 {
                fields.extend([off + 6, off + 7, off + 8]);
            }
            off += 5 + usize::from(u16::from_be_bytes([data[off + 3], data[off + 4]]));
        }
        fields
    }

    /// The client disconnects at any point of the handshake.
    #[ntex::test]
    async fn test_negotiate_truncated() {
        let config = ServerConfig::new(identity()).unwrap();
        for version in tls_versions() {
            let flights = client_flights(&config, version).await;
            let cases = flights
                .iter()
                .enumerate()
                .flat_map(|(flight, data)| {
                    (0..data.len()).map(move |len| Tamper::Truncate { flight, len })
                })
                .collect();
            for (tamper, server, end) in run_cases(&config, version, cases).await {
                assert_eq!(end, ClientEnd::Closed, "{version:?} {tamper:?}");
                assert!(server.is_err(), "{version:?} {tamper:?}");
            }
        }
    }

    /// Any byte of the client's flights is corrupted.
    #[ntex::test]
    async fn test_negotiate_corrupted() {
        let config = ServerConfig::new(identity()).unwrap();
        for version in tls_versions() {
            let flights = client_flights(&config, version).await;
            let positions: Vec<_> = flights
                .iter()
                .enumerate()
                .flat_map(|(flight, data)| (0..data.len()).map(move |pos| (flight, pos)))
                .collect();

            // the connection is closed after the corrupted flight
            let cases = positions
                .iter()
                .map(|&(flight, pos)| Tamper::Flip { flight, pos })
                .collect();
            for (tamper, server, end) in run_cases(&config, version, cases).await {
                assert_eq!(end, ClientEnd::Closed, "{version:?} {tamper:?}");
                // a corrupted random still completes the handshake
                let _ = server;
            }

            // the connection stays open
            let cases = positions
                .iter()
                .map(|&(flight, pos)| Tamper::FlipOpen { flight, pos })
                .collect();
            for (tamper, server, end) in run_cases(&config, version, cases).await {
                let Tamper::FlipOpen { flight, pos } = tamper else {
                    unreachable!()
                };
                if end == ClientEnd::Waiting {
                    // only a longer record or message is waited for
                    assert!(
                        length_fields(&flights[flight]).contains(&pos),
                        "{version:?} {tamper:?}"
                    );
                    assert_eq!(
                        server.unwrap_err().kind(),
                        io::ErrorKind::UnexpectedEof,
                        "{version:?} {tamper:?}"
                    );
                }
            }
        }
    }

    /// A sans-IO rustls peer on the other end of an `IoTest` pair.
    struct Peer {
        io: IoTest,
        conn: tls_rustls::Connection,
        /// Received plaintext
        data: Vec<u8>,
        /// Length of `data` when `close_notify` arrived
        closed_at: Option<usize>,
    }

    impl Peer {
        fn new(io: IoTest, conn: impl Into<tls_rustls::Connection>) -> Self {
            Self {
                io,
                conn: conn.into(),
                data: Vec::new(),
                closed_at: None,
            }
        }

        fn flush(&mut self) {
            let mut out = Vec::new();
            while self.conn.wants_write() {
                self.conn.write_tls(&mut out).unwrap();
            }
            if !out.is_empty() {
                self.io.write(out);
            }
        }

        /// Processes the next read, returns `false` once the transport is closed.
        async fn pump(&mut self) -> bool {
            let data = self.io.read().await.unwrap();
            if data.is_empty() {
                return false;
            }
            let mut rd = &data[..];
            while !rd.is_empty() {
                self.conn.read_tls(&mut rd).unwrap();
                let state = self.conn.process_new_packets().unwrap();
                let _ = self.conn.reader().read_to_end(&mut self.data);
                if state.peer_has_closed() && self.closed_at.is_none() {
                    self.closed_at = Some(self.data.len());
                }
            }
            self.flush();
            true
        }

        async fn handshake(&mut self) {
            self.flush();
            while self.conn.is_handshaking() {
                assert!(self.pump().await, "closed during the handshake");
            }
            self.flush();
        }

        /// Waits for the schannel side's `close_notify`.
        async fn wait_close_notify(&mut self) {
            timeout(Millis(5_000), async {
                while self.closed_at.is_none() {
                    assert!(self.pump().await, "closed without close_notify");
                }
            })
            .await
            .expect("no close_notify");
        }

        fn send(&mut self, data: &[u8]) {
            self.conn.writer().write_all(data).unwrap();
            self.flush();
        }

        fn close_notify(&mut self) {
            self.conn.send_close_notify();
            self.flush();
        }

        fn key_update(&mut self) {
            match &mut self.conn {
                tls_rustls::Connection::Client(conn) => conn.refresh_traffic_keys().unwrap(),
                tls_rustls::Connection::Server(conn) => conn.refresh_traffic_keys().unwrap(),
            }
            self.flush();
        }
    }

    fn rustls_server(
        version: &'static tls_rustls::SupportedProtocolVersion,
    ) -> tls_rustls::ServerConnection {
        let cert = &mut std::io::BufReader::new(&include_bytes!("../../examples/cert.pem")[..]);
        let key = &mut std::io::BufReader::new(&include_bytes!("../../examples/key.pem")[..]);
        let certs = rustls_pemfile::certs(cert)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls_pemfile::private_key(key).unwrap().unwrap();
        let cfg = tls_rustls::ServerConfig::builder_with_provider(
            tls_rustls::crypto::ring::default_provider().into(),
        )
        .with_protocol_versions(&[version])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        tls_rustls::ServerConnection::new(Arc::new(cfg)).unwrap()
    }

    /// Connects a schannel server or client to a rustls peer.
    async fn rustls_pair(
        server: bool,
        version: &'static tls_rustls::SupportedProtocolVersion,
    ) -> (SchannelIo, Peer) {
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let (io, mut peer) = if server {
            let conn = tls_rustls::ClientConnection::new(
                Arc::new(h2_config(version)),
                ServerName::try_from("localhost").unwrap(),
            )
            .unwrap();
            (srv, Peer::new(cli, conn))
        } else {
            (cli, Peer::new(srv, rustls_server(version)))
        };
        let io = Io::new(io, SharedCfg::new("SCHANNEL"));
        let (io, ()) = timeout(Millis(5_000), async {
            if server {
                let config = ServerConfig::new(identity()).unwrap();
                join(accept_io(io, &config), peer.handshake()).await
            } else {
                join(connect(io, "localhost", client_config()), peer.handshake()).await
            }
        })
        .await
        .unwrap();
        // TLS 1.3 session tickets
        sleep(Millis(20)).await;
        (io.unwrap(), peer)
    }

    fn roles() -> [(bool, &'static tls_rustls::SupportedProtocolVersion); 4] {
        [
            (true, &tls_rustls::version::TLS12),
            (true, &tls_rustls::version::TLS13),
            (false, &tls_rustls::version::TLS12),
            (false, &tls_rustls::version::TLS13),
        ]
    }

    /// Shuts down the schannel side, it sends `close_notify` after pending
    /// data and waits for the peer's `close_notify`.
    async fn initiated_shutdown(
        io: SchannelIo,
        peer: &mut Peer,
        data: &[u8],
        key_update: bool,
        case: &str,
    ) {
        io.encode(Bytes::copy_from_slice(data), &BytesCodec)
            .unwrap();
        let done = std::rc::Rc::new(std::cell::RefCell::new(None));
        let done2 = done.clone();
        ntex::rt::spawn(async move {
            let res = io.shutdown().await;
            done2.replace(Some(res.map_err(|e| e.to_string())));
        });

        peer.wait_close_notify().await;
        assert_eq!(peer.closed_at, Some(data.len()), "{case}");
        assert_eq!(peer.data, data, "{case}");

        // the peer's close_notify has not arrived yet
        sleep(Millis(50)).await;
        assert_eq!(done.take(), None, "{case}");

        if key_update {
            // a post-handshake message after our close_notify
            peer.key_update();
            sleep(Millis(20)).await;
            assert_eq!(done.take(), None, "{case}");
        }
        peer.close_notify();
        timeout(Millis(5_000), async {
            while done.borrow().is_none() {
                sleep(Millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|()| panic!("{case}: shutdown does not complete"));
        assert_eq!(done.take(), Some(Ok(())), "{case}");
        // the transport is closed after the exchange
        assert!(!peer.pump().await, "{case}");
    }

    #[ntex::test]
    async fn test_close_notify_initiated() {
        for (server, version) in roles() {
            let case = format!("server {server} {version:?}");
            let (io, mut peer) = rustls_pair(server, version).await;
            initiated_shutdown(io, &mut peer, b"bye", false, &case).await;

            // more data than the write buffer
            let (io, mut peer) = rustls_pair(server, version).await;
            let data = "0123456789abcdef".repeat(32 * 1024);
            initiated_shutdown(io, &mut peer, data.as_bytes(), false, &case).await;
        }
    }

    /// The peer shuts down, the schannel side responds with `close_notify`.
    #[ntex::test]
    async fn test_close_notify_peer_initiated() {
        for (server, version) in roles() {
            let case = format!("server {server} {version:?}");
            let (io, mut peer) = rustls_pair(server, version).await;
            peer.send(b"hello");
            peer.close_notify();

            assert_eq!(
                io.recv(&BytesCodec).await.unwrap().unwrap(),
                "hello",
                "{case}"
            );
            assert!(io.recv(&BytesCodec).await.unwrap().is_none(), "{case}");
            peer.wait_close_notify().await;

            timeout(Millis(5_000), io.shutdown())
                .await
                .unwrap_or_else(|()| panic!("{case}: shutdown does not complete"))
                .unwrap();
            assert!(!peer.pump().await, "{case}");
        }
    }

    /// The schannel side shuts down after a post-handshake key update.
    #[ntex::test]
    async fn test_close_notify_after_key_update() {
        for server in [true, false] {
            let case = format!("server {server}");
            let (io, mut peer) = rustls_pair(server, &tls_rustls::version::TLS13).await;
            peer.key_update();
            sleep(Millis(20)).await;
            initiated_shutdown(io, &mut peer, b"bye", false, &case).await;

            let (io, mut peer) = rustls_pair(server, &tls_rustls::version::TLS13).await;
            initiated_shutdown(io, &mut peer, b"bye", true, &case).await;
        }
    }

    /// The peer shuts down after a post-handshake key update.
    #[ntex::test]
    async fn test_close_notify_peer_after_key_update() {
        for server in [true, false] {
            let case = format!("server {server}");
            let (io, mut peer) = rustls_pair(server, &tls_rustls::version::TLS13).await;
            peer.key_update();
            peer.send(b"hello");
            peer.close_notify();

            assert_eq!(
                io.recv(&BytesCodec).await.unwrap().unwrap(),
                "hello",
                "{case}"
            );
            assert!(io.recv(&BytesCodec).await.unwrap().is_none(), "{case}");
            peer.wait_close_notify().await;
        }
    }

    #[cfg(feature = "openssl")]
    #[derive(Default)]
    struct Mem {
        rd: Vec<u8>,
        wr: Vec<u8>,
    }

    #[cfg(feature = "openssl")]
    impl Read for Mem {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.rd.is_empty() {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(self.rd.len());
            buf[..n].copy_from_slice(&self.rd[..n]);
            self.rd.drain(..n);
            Ok(n)
        }
    }

    #[cfg(feature = "openssl")]
    impl Write for Mem {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.wr.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A sans-IO TLS 1.2 openssl server, it can start a renegotiation.
    #[cfg(feature = "openssl")]
    struct SslPeer {
        io: IoTest,
        ssl: tls_openssl::ssl::SslStream<Mem>,
        data: Vec<u8>,
        closed_at: Option<usize>,
        /// Output is kept in the buffer by `flush`
        hold: bool,
    }

    #[cfg(feature = "openssl")]
    impl SslPeer {
        fn new(io: IoTest) -> Self {
            use tls_openssl::ssl::{Ssl, SslAcceptor, SslMethod, SslVersion};
            use tls_openssl::{pkey::PKey, x509::X509};

            let cert = X509::from_pem(include_bytes!("../../examples/cert.pem")).unwrap();
            let key = PKey::private_key_from_pem(include_bytes!("../../examples/key.pem")).unwrap();
            let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
            builder.set_certificate(&cert).unwrap();
            builder.set_private_key(&key).unwrap();
            builder
                .set_max_proto_version(Some(SslVersion::TLS1_2))
                .unwrap();
            let mut ssl = Ssl::new(builder.build().context()).unwrap();
            ssl.set_accept_state();
            Self {
                io,
                ssl: tls_openssl::ssl::SslStream::new(ssl, Mem::default()).unwrap(),
                data: Vec::new(),
                closed_at: None,
                hold: false,
            }
        }

        fn flush(&mut self) {
            if self.hold {
                return;
            }
            let out = std::mem::take(&mut self.ssl.get_mut().wr);
            if !out.is_empty() {
                self.io.write(out);
            }
        }

        async fn recv(&mut self) -> bool {
            let data = self.io.read().await.unwrap();
            self.ssl.get_mut().rd.extend_from_slice(&data);
            !data.is_empty()
        }

        fn want_read(err: &tls_openssl::ssl::Error) -> bool {
            err.code() == tls_openssl::ssl::ErrorCode::WANT_READ
        }

        async fn handshake(&mut self) {
            loop {
                match self.ssl.do_handshake() {
                    Ok(()) => break self.flush(),
                    Err(err) if Self::want_read(&err) => {
                        self.flush();
                        assert!(self.recv().await, "closed during the handshake");
                    }
                    Err(err) => panic!("{err}"),
                }
            }
        }

        /// Processes buffered records, a renegotiation is driven by reads.
        fn process(&mut self) {
            let mut buf = [0; 16 * 1024];
            loop {
                match self.ssl.ssl_read(&mut buf) {
                    Ok(n) => self.data.extend_from_slice(&buf[..n]),
                    Err(err) if err.code() == tls_openssl::ssl::ErrorCode::ZERO_RETURN => {
                        self.closed_at.get_or_insert(self.data.len());
                        break;
                    }
                    Err(err) if Self::want_read(&err) => break,
                    Err(err) => panic!("{err}"),
                }
            }
            self.flush();
        }

        /// Sends `HelloRequest`.
        fn renegotiate(&mut self) {
            use foreign_types_shared::ForeignTypeRef;

            unsafe extern "C" {
                fn SSL_renegotiate(ssl: *mut openssl_sys::SSL) -> std::ffi::c_int;
            }
            assert_eq!(unsafe { SSL_renegotiate(self.ssl.ssl().as_ptr()) }, 1);
            if let Err(err) = self.ssl.do_handshake() {
                assert!(Self::want_read(&err), "{err}");
            }
            self.flush();
        }

        fn renegotiate_pending(&self) -> bool {
            use foreign_types_shared::ForeignTypeRef;

            unsafe extern "C" {
                fn SSL_renegotiate_pending(ssl: *const openssl_sys::SSL) -> std::ffi::c_int;
            }
            unsafe { SSL_renegotiate_pending(self.ssl.ssl().as_ptr()) != 0 }
        }

        fn close_notify(&mut self) {
            let _ = self.ssl.shutdown();
            self.flush();
        }
    }

    /// Shutdown during a TLS 1.2 renegotiation sends pending data and
    /// `close_notify` once the exchange completes.
    #[cfg(feature = "openssl")]
    #[ntex::test]
    async fn test_close_notify_during_renegotiation() {
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let mut peer = SslPeer::new(srv);
        let io = Io::new(cli, SharedCfg::new("SCHANNEL"));
        let (io, ()) = timeout(
            Millis(5_000),
            join(connect(io, "localhost", client_config()), peer.handshake()),
        )
        .await
        .unwrap();
        let io = io.unwrap();

        // the client answers HelloRequest with ClientHello and waits for the server
        peer.renegotiate();
        assert!(peer.recv().await);
        assert!(!peer.ssl.get_ref().rd.is_empty());

        io.encode(Bytes::from_static(b"bye"), &BytesCodec).unwrap();
        let done = std::rc::Rc::new(std::cell::RefCell::new(None));
        let done2 = done.clone();
        ntex::rt::spawn(async move {
            let res = io.shutdown().await;
            done2.replace(Some(res.map_err(|e| e.to_string())));
        });

        timeout(Millis(5_000), async {
            loop {
                peer.process();
                if peer.closed_at.is_some() {
                    break;
                }
                assert!(peer.recv().await, "closed without close_notify");
            }
        })
        .await
        .expect("no close_notify");
        assert_eq!(peer.closed_at, Some(3));
        assert_eq!(peer.data, b"bye");
        assert_eq!(done.borrow().as_ref(), None);

        peer.close_notify();
        timeout(Millis(5_000), async {
            while done.borrow().is_none() {
                sleep(Millis(5)).await;
            }
        })
        .await
        .expect("shutdown does not complete");
        assert_eq!(done.take(), Some(Ok(())));
    }

    /// Deterministic pseudo-random sizes.
    struct Lcg(u32);

    impl Lcg {
        fn next(&mut self, max: usize) -> usize {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            1 + (self.0 >> 8) as usize % max
        }
    }

    /// Chunk content depends on its tag, index and offset.
    #[allow(clippy::cast_possible_truncation)]
    fn chunk(tag: u8, i: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|j| tag ^ (i as u8).wrapping_mul(31) ^ (j as u8) ^ ((j >> 8) as u8))
            .collect()
    }

    fn chunks(tag: u8, rnd: &mut Lcg) -> (Vec<Bytes>, Vec<u8>) {
        let chunks: Vec<_> = (0..64)
            .map(|i| Bytes::from(chunk(tag, i, rnd.next(12 * 1024))))
            .collect();
        let all = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        (chunks, all)
    }

    fn assert_same(case: &str, what: &str, got: &[u8], expected: &[u8]) {
        let pos = got.iter().zip(expected).position(|(a, b)| a != b);
        assert!(
            got.len() == expected.len() && pos.is_none(),
            "{case}: {what} differs at {pos:?}, {} of {} bytes",
            got.len(),
            expected.len()
        );
    }

    /// Writes `chunks` one by one, reads into the returned buffer.
    fn stream(io: SchannelIo, chunks: Vec<Bytes>) -> std::rc::Rc<std::cell::RefCell<Vec<u8>>> {
        let ioref = io.get_ref();
        ntex::rt::spawn(async move {
            for data in chunks {
                ioref.encode(data, &BytesCodec).unwrap();
                ntex_util::task::yield_to().await;
            }
        });
        let received = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let received2 = received.clone();
        ntex::rt::spawn(async move {
            while let Ok(Some(data)) = io.recv(&BytesCodec).await {
                received2.borrow_mut().extend_from_slice(&data);
            }
        });
        received
    }

    fn drain_tls(conn: &mut tls_rustls::Connection, wire: &mut std::collections::VecDeque<u8>) {
        let mut out = Vec::new();
        while conn.wants_write() {
            conn.write_tls(&mut out).unwrap();
        }
        wire.extend(out);
    }

    /// Data is delivered in order in both directions while the peer's data,
    /// interleaved with key updates, arrives in arbitrary pieces.
    #[ntex::test]
    async fn test_order_key_updates() {
        for server in [true, false] {
            let case = format!("server {server}");
            let (io, mut peer) = rustls_pair(server, &tls_rustls::version::TLS13).await;
            let mut rnd = Lcg(u32::from(server) + 7);

            let mut wire = std::collections::VecDeque::new();
            let (peer_chunks, sent) = chunks(b'p', &mut rnd);
            for (i, data) in peer_chunks.iter().enumerate() {
                peer.conn.writer().write_all(data).unwrap();
                if i % 8 == 7 {
                    match &mut peer.conn {
                        tls_rustls::Connection::Client(c) => c.refresh_traffic_keys().unwrap(),
                        tls_rustls::Connection::Server(c) => c.refresh_traffic_keys().unwrap(),
                    }
                }
                drain_tls(&mut peer.conn, &mut wire);
            }

            let (our_chunks, written) = chunks(b's', &mut rnd);
            let received = stream(io, our_chunks);

            timeout(Millis(10_000), async {
                while received.borrow().len() < sent.len() || peer.data.len() < written.len() {
                    let mut idle = true;
                    if !wire.is_empty() {
                        let n = rnd.next(2048).min(wire.len());
                        peer.io.write(wire.drain(..n).collect::<Vec<_>>());
                        idle = false;
                    }
                    ntex_util::task::yield_to().await;

                    let data = peer.io.read_any();
                    let mut rd = &data[..];
                    while !rd.is_empty() {
                        idle = false;
                        peer.conn.read_tls(&mut rd).unwrap();
                        peer.conn.process_new_packets().unwrap();
                        let _ = peer.conn.reader().read_to_end(&mut peer.data);
                    }
                    // responses go after the queued records
                    drain_tls(&mut peer.conn, &mut wire);
                    if idle {
                        sleep(Millis(1)).await;
                    }
                }
            })
            .await
            .unwrap_or_else(|()| {
                panic!(
                    "{case}: stalled, received {} of {}, peer {} of {}",
                    received.borrow().len(),
                    sent.len(),
                    peer.data.len(),
                    written.len()
                )
            });
            assert_same(&case, "received", &received.borrow(), &sent);
            assert_same(&case, "peer received", &peer.data, &written);
        }
    }

    /// Data is delivered in order in both directions across TLS 1.2
    /// renegotiations started by the server, with and without the server
    /// sending application data during a renegotiation.
    #[cfg(feature = "openssl")]
    #[ntex::test]
    async fn test_order_renegotiation() {
        for during in [false, true] {
            order_renegotiation(during).await;
        }
    }

    #[cfg(feature = "openssl")]
    async fn order_renegotiation(during: bool) {
        let case = format!("during {during}");
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 22);
        srv.remote_buffer_cap(1 << 22);
        let mut peer = SslPeer::new(srv);
        let io = Io::new(cli, SharedCfg::new("SCHANNEL"));
        let (io, ()) = timeout(
            Millis(5_000),
            join(connect(io, "localhost", client_config()), peer.handshake()),
        )
        .await
        .unwrap();

        let mut rnd = Lcg(3);
        let (peer_chunks, sent) = chunks(b'p', &mut rnd);
        let (our_chunks, written) = chunks(b's', &mut rnd);
        let received = stream(io.unwrap(), our_chunks);

        let mut next = 0;
        let mut pending: &[u8] = &[];
        let mut renegotiations = 0;
        timeout(Millis(10_000), async {
            while received.borrow().len() < sent.len() || peer.data.len() < written.len() {
                let renegotiating = peer.renegotiate_pending();
                if pending.is_empty() && next < peer_chunks.len() && !renegotiating {
                    if next % 16 == 8 {
                        peer.renegotiate();
                        renegotiations += 1;
                    }
                    pending = &peer_chunks[next];
                    next += 1;
                }
                if !pending.is_empty() && (during || !peer.renegotiate_pending()) {
                    match peer.ssl.ssl_write(pending) {
                        Ok(n) => pending = &pending[n..],
                        Err(err) if SslPeer::want_read(&err) => {}
                        Err(err) => panic!("{case}: {err}"),
                    }
                }
                peer.flush();
                ntex_util::task::yield_to().await;

                let data = peer.io.read_any();
                let idle = data.is_empty();
                peer.ssl.get_mut().rd.extend_from_slice(&data);
                peer.process();
                if idle {
                    sleep(Millis(1)).await;
                }
            }
        })
        .await
        .unwrap_or_else(|()| {
            panic!(
                "{case}: stalled, received {} of {}, peer {} of {}",
                received.borrow().len(),
                sent.len(),
                peer.data.len(),
                written.len()
            )
        });
        assert_eq!(renegotiations, 4, "{case}");
        assert_same(&case, "received", &received.borrow(), &sent);
        assert_same(&case, "peer received", &peer.data, &written);
    }

    /// Connects a schannel client to an OpenSSL TLS 1.2 server.
    #[cfg(feature = "openssl")]
    async fn openssl_pair() -> (SchannelIo, SslPeer) {
        let (cli, srv) = IoTest::create();
        cli.remote_buffer_cap(1 << 20);
        srv.remote_buffer_cap(1 << 20);
        let mut peer = SslPeer::new(srv);
        let io = Io::new(cli, SharedCfg::new("SCHANNEL"));
        let (io, ()) = timeout(
            Millis(5_000),
            join(connect(io, "localhost", client_config()), peer.handshake()),
        )
        .await
        .unwrap();
        (io.unwrap(), peer)
    }

    /// Receives `expected`, then completes the renegotiation and exchanges
    /// data in both directions.
    #[cfg(feature = "openssl")]
    async fn renegotiation_completes(
        io: &SchannelIo,
        peer: &mut SslPeer,
        expected: &[u8],
        case: &str,
    ) {
        let mut data = Vec::new();
        timeout(Millis(5_000), async {
            while data.len() < expected.len() {
                data.extend_from_slice(&io.recv(&BytesCodec).await.unwrap().unwrap());
            }
        })
        .await
        .unwrap_or_else(|()| panic!("{case}: stalled, received {data:?}"));
        assert_eq!(data, expected, "{case}");

        // writes are held until the renegotiation completes
        io.encode(Bytes::from_static(b"ping"), &BytesCodec).unwrap();
        peer.hold = false;
        peer.flush();
        timeout(Millis(5_000), async {
            while peer.renegotiate_pending() || peer.data.len() < 4 {
                assert!(peer.recv().await, "{case}: closed");
                peer.process();
            }
        })
        .await
        .unwrap_or_else(|()| panic!("{case}: renegotiation stalled"));
        assert_eq!(peer.data, b"ping", "{case}");

        peer.ssl.ssl_write(b"after").unwrap();
        peer.flush();
        let after = timeout(Millis(5_000), io.recv(&BytesCodec)).await;
        assert_eq!(after.unwrap().unwrap().unwrap(), "after", "{case}");
    }

    /// Application data the peer sends after a TLS 1.2 `HelloRequest` is
    /// delivered in order and the renegotiation completes.
    #[cfg(feature = "openssl")]
    #[ntex::test]
    async fn test_renegotiation_app_data() {
        // the peer's output as written, all at once and byte by byte
        for chunk in [None, Some(usize::MAX), Some(1)] {
            let (io, mut peer) = openssl_pair().await;
            peer.hold = chunk.is_some();
            peer.ssl.ssl_write(b"before").unwrap();
            peer.renegotiate();
            peer.ssl.ssl_write(b"during").unwrap();
            peer.flush();
            if let Some(chunk) = chunk {
                let out = std::mem::take(&mut peer.ssl.get_mut().wr);
                for piece in out.chunks(chunk) {
                    peer.io.write(piece);
                    ntex_util::task::yield_to().await;
                }
            }
            renegotiation_completes(&io, &mut peer, b"beforeduring", &format!("{chunk:?}")).await;
        }
    }

    /// Application data read together with the server's handshake flight
    /// of a TLS 1.2 renegotiation.
    #[cfg(feature = "openssl")]
    #[ntex::test]
    async fn test_renegotiation_app_data_with_flight() {
        let (io, mut peer) = openssl_pair().await;
        peer.ssl.ssl_write(b"before").unwrap();
        peer.renegotiate();

        // the ClientHello
        assert!(timeout(Millis(5_000), peer.recv()).await.unwrap());
        peer.hold = true;
        peer.ssl.ssl_write(b"during").unwrap();
        peer.process();
        assert!(peer.renegotiate_pending());
        peer.hold = false;
        peer.flush();
        renegotiation_completes(&io, &mut peer, b"beforeduring", "flight").await;
    }

    /// The peer closes the connection during a TLS 1.2 renegotiation.
    #[cfg(feature = "openssl")]
    #[ntex::test]
    async fn test_renegotiation_peer_close_notify() {
        for (data, split) in [(false, false), (true, false), (false, true), (true, true)] {
            let case = format!("data {data} split {split}");
            let (io, mut peer) = openssl_pair().await;
            peer.hold = true;
            peer.ssl.ssl_write(b"before").unwrap();
            peer.renegotiate();
            if data {
                peer.ssl.ssl_write(b"during").unwrap();
            }
            peer.hold = false;
            if split {
                // the ClientHello is sent before close_notify arrives
                peer.flush();
                assert!(timeout(Millis(5_000), peer.recv()).await.unwrap());
            }
            peer.close_notify();

            let res = timeout(Millis(5_000), async {
                let mut received = Vec::new();
                loop {
                    match io.recv(&BytesCodec).await {
                        Ok(Some(chunk)) => received.extend_from_slice(&chunk),
                        Ok(None) => break Ok(received),
                        Err(err) => break Err((received, err.into_inner().to_string())),
                    }
                }
            })
            .await
            .unwrap_or_else(|()| panic!("{case}: stalled"));
            let expected: &[u8] = if data { b"beforeduring" } else { b"before" };
            assert_eq!(res.unwrap(), expected, "{case}");
            timeout(Millis(5_000), io.shutdown())
                .await
                .unwrap_or_else(|()| panic!("{case}: shutdown does not complete"))
                .unwrap();
        }
    }
}
