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

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert<'a>(pub CertificateDer<'a>);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain<'a>(pub Vec<CertificateDer<'a>>);

/// Drive the handshake until the session stops handshaking.
///
/// The filter writes handshake records to the output buffer, the client
/// hello is written when the filter is added.
async fn handshake<F>(io: &Io<F>, handshaking: impl Fn() -> bool) -> io::Result<()> {
    let mut eof = false;
    loop {
        io.flush(false).await?;
        if !handshaking() {
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
    use std::io::{Read, Write};
    use std::{cell::RefCell, rc::Rc, sync::Arc};

    use ntex::codec::BytesCodec;
    use ntex_bytes::Bytes;
    use ntex_error::Error;
    use ntex_io::{IoBoxed, Layer, testing::IoTest, types::HttpProtocol};
    use ntex_net::connect::{Connect, ConnectError, Connector};
    use ntex_service::{Pipeline, cfg::SharedCfg, fn_service};
    use ntex_util::{future::join, future::lazy, time::sleep, time::timeout};
    use tls_rustls::client::UnbufferedClientConnection;
    use tls_rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tls_rustls::pki_types::{ServerName, UnixTime};
    use tls_rustls::unbuffered::{ConnectionState, UnbufferedStatus};
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

    /// Completes a handshake of an unbuffered client with a buffered server
    /// in memory.
    fn unbuffered_client() -> (UnbufferedClientConnection, tls_rustls::ServerConnection) {
        let mut client = UnbufferedClientConnection::new(
            client_config(false),
            ServerName::try_from("localhost").unwrap(),
        )
        .unwrap();
        let mut server = tls_rustls::ServerConnection::new(server_config(false)).unwrap();
        let mut incoming = Vec::new();
        loop {
            let mut out = Vec::new();
            let traffic = loop {
                let UnbufferedStatus { discard, state } = client.process_tls_records(&mut incoming);
                let next = match state.unwrap() {
                    ConnectionState::EncodeTlsData(mut state) => {
                        let mut buf = vec![0; 32 * 1024];
                        let n = state.encode(&mut buf).unwrap();
                        out.extend_from_slice(&buf[..n]);
                        None
                    }
                    ConnectionState::TransmitTlsData(state) => {
                        state.done();
                        None
                    }
                    ConnectionState::WriteTraffic(_) => Some(true),
                    ConnectionState::BlockedHandshake => Some(false),
                    _ => panic!("unexpected state"),
                };
                incoming.drain(..discard);
                if let Some(traffic) = next {
                    break traffic;
                }
            };
            let mut rd = &out[..];
            while !rd.is_empty() {
                server.read_tls(&mut rd).unwrap();
            }
            server.process_new_packets().unwrap();
            while server.wants_write() {
                server.write_tls(&mut incoming).unwrap();
            }
            if traffic && incoming.is_empty() && !server.is_handshaking() {
                return (client, server);
            }
        }
    }

    /// Small pages are joined with the following pages into one record.
    #[test]
    #[allow(clippy::assert_is_empty)]
    fn write_gathers_pages_into_records() {
        use std::io::Read;

        use ntex_bytes::{BytePageSize, BytePages};

        let limit = BytePageSize::Size16.capacity();
        let (mut client, mut server) = unbuffered_client();

        for (sizes, expected) in [
            (&[300, 8192][..], 1),
            (&[300, limit - 300 - stream::OVERHEAD][..], 1),
            (&[16384][..], 1),
            (&[300, 16384][..], 2),
            (&[100, 5000, 6000, 7000][..], 2),
            (&[9000, 6000][..], 2),
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
            let ConnectionState::WriteTraffic(mut state) =
                client.process_tls_records(&mut []).state.unwrap()
            else {
                panic!("handshake is not complete");
            };
            stream::encrypt_pages(&mut state, &mut src, &mut dst).unwrap();
            assert!(src.is_empty());

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
    type Version = &'static tls_rustls::SupportedProtocolVersion;

    fn configs(
        version: Version,
        fragment: Option<usize>,
    ) -> (Arc<ClientConfig>, Arc<ServerConfig>) {
        let provider = Arc::new(tls_rustls::crypto::ring::default_provider());
        let mut client = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[version])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify))
            .with_no_client_auth();
        client.max_fragment_size = fragment;

        let certs = rustls_pemfile::certs(&mut &CERT[..])
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls_pemfile::private_key(&mut &KEY[..]).unwrap().unwrap();
        let mut server = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[version])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        server.max_fragment_size = fragment;
        (Arc::new(client), Arc::new(server))
    }

    /// A sans-IO rustls peer on the other end of an `IoTest` pair, its
    /// output is delivered in `chunk` sized pieces.
    struct Peer {
        io: IoTest,
        conn: tls_rustls::Connection,
        chunk: usize,
        /// Received plaintext
        data: Vec<u8>,
        closed: bool,
    }

    impl Peer {
        fn new(io: IoTest, conn: impl Into<tls_rustls::Connection>) -> Self {
            Self {
                io,
                conn: conn.into(),
                chunk: 1,
                data: Vec::new(),
                closed: false,
            }
        }

        fn output(&mut self) -> Vec<u8> {
            let mut out = Vec::new();
            while self.conn.wants_write() {
                self.conn.write_tls(&mut out).unwrap();
            }
            out
        }

        /// Writes `data` in pieces, the filter reads each piece separately.
        async fn deliver(&self, data: &[u8], chunk: usize) {
            for piece in data.chunks(chunk) {
                self.io.write(piece);
                for _ in 0..100 {
                    if self.io.remote_buffer(|buf| buf.is_empty()) {
                        break;
                    }
                    ntex_util::task::yield_to().await;
                }
            }
        }

        async fn flush(&mut self) {
            let out = self.output();
            self.deliver(&out, self.chunk).await;
        }

        /// Processes the next read, returns `false` once the transport is closed.
        async fn pump(&mut self, flush: bool) -> bool {
            let data = self.io.read().await.unwrap();
            if data.is_empty() {
                return false;
            }
            self.feed(&data);
            if flush {
                self.flush().await;
            }
            true
        }

        fn feed(&mut self, mut data: &[u8]) {
            while !data.is_empty() {
                self.conn.read_tls(&mut data).unwrap();
                let state = self.conn.process_new_packets().unwrap();
                let _ = self.conn.reader().read_to_end(&mut self.data);
                self.closed |= state.peer_has_closed();
            }
        }

        async fn handshake(&mut self) {
            self.flush().await;
            while self.conn.is_handshaking() {
                assert!(self.pump(true).await, "closed during the handshake");
            }
            self.flush().await;
        }

        async fn recv(&mut self, len: usize) {
            while self.data.len() < len {
                assert!(self.pump(true).await, "closed before data is received");
            }
        }

        async fn send(&mut self, data: &[u8]) {
            self.conn.writer().write_all(data).unwrap();
            self.flush().await;
        }
    }

    /// Connects a filter to a rustls peer, the peer's handshake records are
    /// delivered byte by byte.
    async fn peer_pair(server: bool, version: Version, fragment: Option<usize>) -> (IoBoxed, Peer) {
        let (cli, srv) = pair();
        let (client_cfg, server_cfg) = configs(version, fragment);
        if server {
            let conn = tls_rustls::ClientConnection::new(
                client_cfg,
                ServerName::try_from("LocalHost").unwrap(),
            )
            .unwrap();
            let mut peer = Peer::new(cli, conn);
            let res = join(
                TlsServerFilter::create(
                    Io::new(srv, SharedCfg::new("SRV")),
                    server_cfg,
                    Millis(5_000),
                ),
                peer.handshake(),
            )
            .await;
            (res.0.unwrap().into(), peer)
        } else {
            let mut peer = Peer::new(srv, tls_rustls::ServerConnection::new(server_cfg).unwrap());
            let res = join(
                TlsClientFilter::create(
                    Io::new(cli, SharedCfg::new("CLI")),
                    client_cfg,
                    ServerName::try_from("localhost").unwrap(),
                ),
                peer.handshake(),
            )
            .await;
            (res.0.unwrap().into(), peer)
        }
    }

    fn roles() -> [(bool, Version); 4] {
        [
            (true, &tls_rustls::version::TLS12),
            (true, &tls_rustls::version::TLS13),
            (false, &tls_rustls::version::TLS12),
            (false, &tls_rustls::version::TLS13),
        ]
    }

    async fn recv_exact(io: &IoBoxed, len: usize) -> Vec<u8> {
        let mut data = Vec::new();
        while data.len() < len {
            data.extend_from_slice(&io.recv(&BytesCodec).await.unwrap().unwrap());
        }
        data
    }

    /// Records split at any byte, handshake messages fragmented into small
    /// records, and key updates.
    #[ntex::test]
    async fn peer_exchange() {
        for (server, version) in roles() {
            for fragment in [None, Some(64)] {
                let case = format!("server: {server} {version:?} {fragment:?}");
                let (io, mut peer) = peer_pair(server, version, fragment).await;
                if server {
                    // server name of the split client hello
                    assert_eq!(
                        io.query::<Servername>().as_ref().map(|s| s.0.as_str()),
                        Some("localhost"),
                        "{case}"
                    );
                }

                let data: Vec<u8> = (0..=255u8).cycle().take(40_000).collect();
                peer.chunk = 7;
                peer.send(&data).await;
                assert_eq!(recv_exact(&io, data.len()).await, data, "{case}");

                let data: Vec<u8> = (0..=250u8).rev().cycle().take(70_000).collect();
                io.send(Bytes::from(data.clone()), &BytesCodec)
                    .await
                    .unwrap();
                peer.recv(data.len()).await;
                assert_eq!(peer.data, data, "{case}");
                peer.data.clear();

                if version == &tls_rustls::version::TLS13 {
                    match &mut peer.conn {
                        tls_rustls::Connection::Client(conn) => conn.refresh_traffic_keys(),
                        tls_rustls::Connection::Server(conn) => conn.refresh_traffic_keys(),
                    }
                    .unwrap();
                    peer.send(b"after update").await;
                    assert_eq!(recv_exact(&io, 12).await, b"after update", "{case}");
                    // the KeyUpdate response is sent without application data
                    let res = timeout(Millis(1_000), peer.pump(false)).await;
                    assert_eq!(res, Ok(true), "{case}");

                    io.send(Bytes::from_static(b"reply"), &BytesCodec)
                        .await
                        .unwrap();
                    peer.recv(5).await;
                    assert_eq!(peer.data, b"reply", "{case}");
                }

                // peer initiated close
                peer.conn.send_close_notify();
                peer.flush().await;
                assert!(io.recv(&BytesCodec).await.unwrap().is_none(), "{case}");
                io.shutdown().await.unwrap();
                while !peer.closed {
                    assert!(peer.pump(false).await, "{case}");
                }
            }
        }
    }

    /// Application data is written while a post-handshake message is
    /// partially received.
    #[ntex::test]
    async fn write_during_split_handshake_message() {
        let (cli, srv) = pair();
        let (client_cfg, server_cfg) = configs(&tls_rustls::version::TLS13, Some(64));
        let mut peer = Peer::new(srv, tls_rustls::ServerConnection::new(server_cfg).unwrap());
        let (io, ()) = join(
            TlsClientFilter::create(
                Io::new(cli, SharedCfg::new("CLI")),
                client_cfg,
                ServerName::try_from("localhost").unwrap(),
            ),
            async {
                peer.flush().await;
                while peer.conn.is_handshaking() {
                    // session tickets are held
                    assert!(peer.pump(!peer.conn.is_handshaking()).await);
                    if peer.conn.is_handshaking() {
                        peer.flush().await;
                    }
                }
            },
        )
        .await;
        let io = IoBoxed::from(io.unwrap());

        let tickets = peer.output();
        let mut rest = &tickets[..];
        let mut records = 0;
        while !rest.is_empty() {
            let len = 5 + usize::from(u16::from_be_bytes([rest[3], rest[4]]));
            peer.deliver(&rest[..len], len).await;
            rest = &rest[len..];
            io.send(Bytes::from_static(b"ping"), &BytesCodec)
                .await
                .unwrap();
            records += 1;
        }
        assert!(records > 2, "{records}");

        peer.recv(4 * records).await;
        assert_eq!(peer.data, b"ping".repeat(records));
        peer.send(b"pong").await;
        assert_eq!(recv_exact(&io, 4).await, b"pong");
    }

    /// A corrupted record fails the connection.
    #[ntex::test]
    async fn invalid_record() {
        for (server, version) in roles() {
            let (io, peer) = peer_pair(server, version, None).await;
            let mut record = vec![23, 3, 3, 0, 32];
            record.extend_from_slice(&[0; 32]);
            peer.io.write(&record);
            let err = io.recv(&BytesCodec).await.unwrap_err();
            assert_eq!(
                err.right().unwrap().kind(),
                io::ErrorKind::InvalidData,
                "{server} {version:?}"
            );
        }
    }
}
