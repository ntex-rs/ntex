//! An implementation of SSL streams for ntex backed by OpenSSL
use std::{any, borrow::ToOwned, cell::UnsafeCell, cmp, io, ptr, task::Poll};

use foreign_types_shared::ForeignType;
use ntex_bytes::{BufMut, BytePage, BytePages, BytesMut};
use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer, types};
use openssl_sys as ffi;
use tls_openssl::ssl::{self, NameType, SslStream};
use tls_openssl::x509::X509;

use crate::{PeerCertChainDer, PeerCertDer, PskIdentity, Servername};

mod connect;
pub use self::connect::SslConnector;

mod accept;
pub use self::accept::SslAcceptor;

mod alloc;
pub use self::alloc::use_global_allocator;

/// Connection's peer cert
#[derive(Debug)]
pub struct PeerCert(pub X509);

/// Connection's peer cert chain
#[derive(Debug)]
pub struct PeerCertChain(pub Vec<X509>);

/// An implementation of SSL streams
#[derive(Debug)]
pub struct SslFilter {
    inner: UnsafeCell<SslStream<IoInner>>,
}

#[derive(Debug)]
struct IoInner {
    source: Option<BytesMut>,
    destination: BytePages,
}

impl io::Read for IoInner {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        if let Some(ref mut buf) = self.source {
            if buf.is_empty() {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                let len = cmp::min(buf.len(), dst.len());
                dst[..len].copy_from_slice(&buf[..len]);
                buf.advance_to(len);
                Ok(len)
            }
        } else {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }
}

impl io::Write for IoInner {
    fn write(&mut self, src: &[u8]) -> io::Result<usize> {
        self.destination.extend_from_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SslFilter {
    fn new(stream: SslStream<IoInner>) -> Self {
        Self {
            inner: UnsafeCell::new(stream),
        }
    }

    fn ssl(&self) -> &ssl::SslRef {
        // SAFETY: the filter is single-threaded, and a mutable reference to
        // the stream exists only inside `with_buffers`, which never calls back
        // into the filter.
        unsafe { (*self.inner.get()).ssl() }
    }

    fn with_buffers<F, R>(&self, buf: &FilterBuf<'_>, f: F) -> R
    where
        F: FnOnce(&mut SslStream<IoInner>, &FilterBuf<'_>) -> R,
    {
        self.with_buffers_inner(buf, true, f)
    }

    /// Runs `f` for input processing.
    ///
    /// The current write page is not borrowed: putting it back would look
    /// like output produced by reading, which pauses reads while the write
    /// buffer is full. Output produced here, such as alerts or key updates,
    /// is rare and small.
    fn with_read_buffers<F, R>(&self, buf: &FilterBuf<'_>, f: F) -> R
    where
        F: FnOnce(&mut SslStream<IoInner>, &FilterBuf<'_>) -> R,
    {
        self.with_buffers_inner(buf, false, f)
    }

    fn with_buffers_inner<F, R>(&self, buf: &FilterBuf<'_>, reuse_page: bool, f: F) -> R
    where
        F: FnOnce(&mut SslStream<IoInner>, &FilterBuf<'_>) -> R,
    {
        // SAFETY: see `ssl()`. Neither the BIO callbacks nor the buffer
        // operations below re-enter the filter.
        let stream = unsafe { &mut *self.inner.get() };

        let st = stream.get_mut();
        st.source = buf.with_read_src(Option::take);

        // get current page from destination buffer (optimization)
        if reuse_page {
            buf.with_write_buffers(|_, dst| st.destination.try_get_current_from(dst));
        }

        let result = f(stream, buf);

        let st = stream.get_mut();
        // an empty source goes back to the read buffer cache
        if let Some(src) = st.source.take() {
            buf.with_read_src(|buf| *buf = Some(src));
        }

        // copy internal buffer to write dst buffer
        if !st.destination.is_empty() {
            buf.with_write_buffers(|_, dst| {
                st.destination.move_to(dst);
            });
        }
        result
    }
}

impl FilterLayer for SslFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        if id == any::TypeId::of::<types::HttpProtocol>() {
            let alpn = self.ssl().selected_alpn_protocol();
            Some(Box::new(crate::utils::http_protocol(alpn)))
        } else if id == any::TypeId::of::<PeerCert>() {
            Some(Box::new(PeerCert(self.ssl().peer_certificate()?)))
        } else if id == any::TypeId::of::<PeerCertChain>() {
            let chain = self.ssl().peer_cert_chain()?;
            Some(Box::new(PeerCertChain(
                chain.iter().map(ToOwned::to_owned).collect(),
            )))
        } else if id == any::TypeId::of::<PeerCertDer>() {
            let cert = self.ssl().peer_certificate()?;
            Some(Box::new(PeerCertDer(cert.to_der().ok()?)))
        } else if id == any::TypeId::of::<PeerCertChainDer>() {
            let ssl = self.ssl();
            let mut chain = Vec::new();
            for cert in ssl.peer_cert_chain().into_iter().flatten() {
                chain.push(cert.to_der().ok()?);
            }
            // the chain does not include the client's certificate on the
            // server side
            if let Some(cert) = ssl.peer_certificate() {
                let cert = cert.to_der().ok()?;
                if chain.first() != Some(&cert) {
                    chain.insert(0, cert);
                }
            }
            if chain.is_empty() {
                None
            } else {
                Some(Box::new(PeerCertChainDer(chain)))
            }
        } else if id == any::TypeId::of::<Servername>() {
            let name = self.ssl().servername(NameType::HOST_NAME)?;
            Some(Box::new(Servername(name.to_string())))
        } else if id == any::TypeId::of::<PskIdentity>() {
            let psk_id = self.ssl().psk_identity()?;
            Some(Box::new(PskIdentity(psk_id.to_vec())))
        } else {
            None
        }
    }

    fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        let ssl_result = self.with_buffers(buf, |s, _| s.shutdown());
        let result = match ssl_result {
            Ok(ssl::ShutdownResult::Sent) => Ok(Poll::Pending),
            Ok(ssl::ShutdownResult::Received) => Ok(Poll::Ready(())),
            Err(ref e) if e.code() == ssl::ErrorCode::ZERO_RETURN => Ok(Poll::Ready(())),
            Err(ref e)
                if matches!(
                    e.code(),
                    ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE
                ) =>
            {
                Ok(Poll::Pending)
            }
            Err(e) => Err(e.into_io_error().unwrap_or_else(io::Error::other)),
        };

        // Our close_notify has been sent, but the peer closed the connection
        // without sending its own; it is never going to arrive.
        if matches!(result, Ok(Poll::Pending)) && buf.io().is_read_eof() {
            return Ok(Poll::Ready(()));
        }
        result
    }

    fn process_read_buf(&self, rb: &FilterBuf<'_>) -> io::Result<()> {
        self.with_read_buffers(rb, |stream, buf| {
            buf.with_read_buffers(|_, dst| {
                loop {
                    if dst.remaining_mut() == 0 {
                        dst.reserve_more();
                    }

                    let chunk = dst.chunk_mut();
                    match stream.ssl_read_uninit(chunk.as_mut()) {
                        Ok(v) => unsafe { dst.advance_mut(v) },
                        Err(e) => {
                            return match e.code() {
                                ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE => Ok(()),
                                ssl::ErrorCode::ZERO_RETURN => {
                                    rb.io().close();
                                    Ok(())
                                }
                                _ => {
                                    log::trace!("{}: SSL Error: {:?}", rb.tag(), e);
                                    Err(io::Error::other(e))
                                }
                            };
                        }
                    }
                }
            })
        })
    }

    fn process_write_buf(&self, wb: &FilterBuf<'_>) -> io::Result<()> {
        self.with_buffers(wb, |stream, buf| {
            buf.with_write_buffers(|w_src, _| write_pages(stream, w_src))
        })
    }
}

/// Maximum plaintext size of a TLS record.
const MAX_RECORD: usize = 16 * 1024;

fn write_pages(stream: &mut SslStream<IoInner>, src: &mut BytePages) -> io::Result<()> {
    while let Some(page) = src.take() {
        let mut page = gather(page, src);
        match stream.ssl_write(&page) {
            Ok(v) => {
                page.advance_to(v);
                src.prepend(page);
            }
            Err(e)
                if matches!(
                    e.code(),
                    ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE
                ) =>
            {
                // nothing is consumed, e.g. a handshake is in
                // progress, the write is retried later
                src.prepend(page);
                break;
            }
            Err(e) => return Err(io::Error::other(e)),
        }
    }
    Ok(())
}

/// Joins `page` with the following whole pages that fit into one record.
///
/// Every `ssl_write` call produces its own record, so a small page, e.g.
/// response headers in front of a body, would otherwise cost a record.
fn gather(page: BytePage, src: &mut BytePages) -> BytePage {
    let mut buf: Option<BytesMut> = None;
    while let Some(next) = src.take() {
        let len = buf.as_ref().map_or(page.len(), BytesMut::len);
        if len + next.len() > MAX_RECORD {
            src.prepend(next);
            break;
        }
        buf.get_or_insert_with(|| {
            let mut buf = BytesMut::with_capacity(MAX_RECORD);
            buf.extend_from_slice(&page);
            buf
        })
        .extend_from_slice(&next);
    }
    buf.map_or(page, BytePage::from)
}

fn new_stream<F>(io: &Io<F>, ssl: ssl::Ssl) -> io::Result<SslStream<IoInner>> {
    // Let OpenSSL pull all buffered ciphertext in one BIO read, instead of
    // reading every record header and body separately.
    unsafe {
        ffi::SSL_ctrl(
            ssl.as_ptr(),
            ffi::SSL_CTRL_SET_READ_AHEAD,
            1,
            ptr::null_mut(),
        );
    }

    let inner = IoInner {
        source: None,
        destination: BytePages::new(io.cfg().write_size()),
    };
    Ok(SslStream::new(ssl, inner)?)
}

/// Create openssl connector filter factory
pub async fn connect<F: Filter>(
    io: Io<F>,
    ssl: ssl::Ssl,
) -> Result<Io<Layer<SslFilter, F>>, io::Error> {
    handshake(io, ssl, false).await
}

/// Add ssl filter to the io stream and drive the handshake to completion
async fn handshake<F: Filter>(
    io: Io<F>,
    mut ssl: ssl::Ssl,
    accept: bool,
) -> io::Result<Io<Layer<SslFilter, F>>> {
    if accept {
        ssl.set_accept_state();
    } else {
        ssl.set_connect_state();
    }
    let stream = new_stream(&io, ssl)?;
    let io = io.add_filter(SslFilter::new(stream));

    let mut eof = false;
    loop {
        let result = io.with_buf(|buf| io.filter().with_buffers(buf, |s, _| s.do_handshake()))?;
        match result {
            Ok(()) => return Ok(io),
            Err(e) => match e.code() {
                ssl::ErrorCode::WANT_READ => {
                    if eof {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "disconnected"));
                    }
                    // The read that reports eof may also carry the peer's last
                    // handshake flight, so the handshake is stepped once more
                    // before the eof is treated as a failure.
                    eof = io.read_notify().await?.is_none();
                }
                ssl::ErrorCode::WANT_WRITE => {}
                _ => return Err(io::Error::other(e)),
            },
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ntex::codec::BytesCodec;
    use ntex_bytes::Bytes;
    use ntex_io::{IoConfig, testing::IoTest};
    use ntex_service::cfg::SharedCfg;
    use ntex_util::{future::join, time::Millis};
    use tls_openssl::pkey::{PKey, Private};
    use tls_openssl::ssl::{SslMethod, SslVerifyMode};

    use super::*;

    const CERT: &[u8] = include_bytes!("../../examples/cert.pem");
    const KEY: &[u8] = include_bytes!("../../examples/key.pem");

    fn acceptor(alpn: bool) -> ssl::SslAcceptor {
        let mut acceptor = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor
            .set_private_key(&PKey::private_key_from_pem(KEY).unwrap())
            .unwrap();
        acceptor
            .set_certificate(&X509::from_pem(CERT).unwrap())
            .unwrap();
        if alpn {
            acceptor.set_alpn_select_callback(|_, protos| {
                ssl::select_next_proto(b"\x02h2", protos).ok_or(ssl::AlpnError::NOACK)
            });
        }
        acceptor.build()
    }

    fn connector(alpn: bool) -> ssl::SslConnector {
        let mut connector = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);
        if alpn {
            connector.set_alpn_protos(b"\x02h2").unwrap();
        }
        connector.build()
    }

    /// Leaf, intermediate and root certificates with their keys.
    pub(crate) fn cert_chain() -> [(X509, PKey<Private>); 3] {
        use tls_openssl::x509::{X509Builder, X509NameBuilder, extension::BasicConstraints};
        use tls_openssl::{asn1::Asn1Time, bn::BigNum, ec, hash::MessageDigest, nid::Nid};

        let issue = |cn: &str, serial: u32, ca: bool, issuer: Option<&(X509, PKey<Private>)>| {
            let group = ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            let key = PKey::from_ec_key(ec::EcKey::generate(&group).unwrap()).unwrap();
            let mut name = X509NameBuilder::new().unwrap();
            name.append_entry_by_text("CN", cn).unwrap();
            let name = name.build();
            let mut builder = X509Builder::new().unwrap();
            builder.set_version(2).unwrap();
            let serial = BigNum::from_u32(serial).unwrap().to_asn1_integer().unwrap();
            builder.set_serial_number(&serial).unwrap();
            builder.set_subject_name(&name).unwrap();
            builder.set_pubkey(&key).unwrap();
            builder
                .set_not_before(&Asn1Time::days_from_now(0).unwrap())
                .unwrap();
            builder
                .set_not_after(&Asn1Time::days_from_now(1).unwrap())
                .unwrap();
            if ca {
                let ca = BasicConstraints::new().critical().ca().build().unwrap();
                builder.append_extension(ca).unwrap();
            }
            let (issuer, signer) = issuer.map_or((&*name, &key), |(c, k)| (c.subject_name(), k));
            builder.set_issuer_name(issuer).unwrap();
            builder.sign(signer, MessageDigest::sha256()).unwrap();
            (builder.build(), key)
        };
        let root = issue("ntex root", 1, true, None);
        let intermediate = issue("ntex intermediate", 2, true, Some(&root));
        let leaf = issue("ntex leaf", 3, false, Some(&intermediate));
        [leaf, intermediate, root]
    }

    fn pair() -> (IoTest, IoTest) {
        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1 << 20);
        server.remote_buffer_cap(1 << 20);
        (client, server)
    }

    fn tls_cfg(timeout: Millis) -> crate::TlsConfig {
        crate::TlsConfig {
            handshake_timeout: timeout,
            ..crate::TlsConfig::default()
        }
    }

    async fn handshake_pair() -> (Io<Layer<SslFilter>>, Io<Layer<SslFilter>>) {
        let (client, server) = pair();
        let (client, server) = join(
            connect(
                Io::new(client, SharedCfg::new("CLI")),
                connector(false)
                    .configure()
                    .unwrap()
                    .into_ssl("localhost")
                    .unwrap(),
            ),
            handshake(
                Io::new(server, SharedCfg::new("SRV")),
                ssl::Ssl::new(acceptor(false).context()).unwrap(),
                true,
            ),
        )
        .await;
        (client.unwrap(), server.unwrap())
    }

    /// The chain starts with the peer's certificate on both sides.
    #[ntex::test]
    async fn peer_cert_der() {
        let [leaf, intermediate, _] = cert_chain();
        let der = [&leaf.0, &intermediate.0].map(|c| c.to_der().unwrap());

        let mut acceptor = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor.set_private_key(&leaf.1).unwrap();
        acceptor.set_certificate(&leaf.0).unwrap();
        acceptor
            .add_extra_chain_cert(intermediate.0.clone())
            .unwrap();
        acceptor.set_verify_callback(SslVerifyMode::PEER, |_, _| true);
        let acceptor = acceptor.build();

        let mut connector = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);
        connector.set_private_key(&leaf.1).unwrap();
        connector.set_certificate(&leaf.0).unwrap();
        connector.add_extra_chain_cert(intermediate.0).unwrap();
        let ssl = connector
            .build()
            .configure()
            .unwrap()
            .into_ssl("localhost")
            .unwrap();

        let (client, server) = pair();
        let (client, server) = join(
            connect(Io::new(client, SharedCfg::new("CLI")), ssl),
            handshake(
                Io::new(server, SharedCfg::new("SRV")),
                ssl::Ssl::new(acceptor.context()).unwrap(),
                true,
            ),
        )
        .await;
        for io in [client.unwrap(), server.unwrap()] {
            let cert = io.query::<PeerCertDer>();
            assert_eq!(cert.as_ref().map(|c| &c.0), Some(&der[0]));
            let chain = io.query::<PeerCertChainDer>();
            assert_eq!(chain.as_ref().map(|c| &c.0[..]), Some(&der[..]));
        }
    }

    #[ntex::test]
    async fn acceptor_and_connector() {
        use std::{cell::RefCell, rc::Rc};

        use ntex_error::Error;
        use ntex_net::connect::{Connect, ConnectError, Connector};
        use ntex_service::{Pipeline, fn_service};

        let (client, server) = pair();
        let client = Rc::new(RefCell::new(Some(Io::new(client, SharedCfg::new("CLI")))));

        let acceptor = SslAcceptor::from(acceptor(true)).clone();
        assert!(format!("{acceptor:?}").contains("SslAcceptor"));
        let acceptor = Pipeline::new((), acceptor);

        let connector = SslConnector::<Connector<&str>>::new(connector(true)).connector(
            fn_service(async move |_: Connect<&str>| {
                Ok::<_, Error<ConnectError>>(client.borrow_mut().take().unwrap())
            }),
        );
        let connector = Pipeline::new(SharedCfg::new("CLI").build(), connector);

        let (server, client) = join(
            acceptor.call(Io::new(server, SharedCfg::new("SRV"))),
            connector.call(Connect::new("localhost:443")),
        )
        .await;
        let (server, client) = (server.unwrap(), client.unwrap());

        assert_eq!(
            client.query::<types::HttpProtocol>().as_ref(),
            Some(&types::HttpProtocol::Http2)
        );
        assert!(client.query::<PeerCert>().as_ref().is_some());
        assert_eq!(
            client.query::<PeerCertChain>().as_ref().map(|c| c.0.len()),
            Some(1)
        );
        assert!(client.query::<PskIdentity>().as_ref().is_none());
        assert_eq!(
            server.query::<Servername>().as_ref().map(|s| s.0.as_str()),
            Some("localhost")
        );
        // no client auth
        assert!(server.query::<PeerCert>().as_ref().is_none());
        assert!(server.query::<PeerCertChain>().as_ref().is_none());
        assert!(server.query::<PeerCertDer>().as_ref().is_none());
        assert!(server.query::<PeerCertChainDer>().as_ref().is_none());
        assert!(server.query::<u32>().as_ref().is_none());

        // larger than the read buffer
        let data = Bytes::from(vec![b'a'; 256 * 1024]);
        client.send(data.clone(), &BytesCodec).await.unwrap();
        let mut received = 0;
        while received < data.len() {
            received += server.recv(&BytesCodec).await.unwrap().unwrap().len();
        }

        // close_notify is exchanged in both directions
        let (res, ()) = join(client.shutdown(), async {
            assert!(server.recv(&BytesCodec).await.unwrap().is_none());
        })
        .await;
        res.unwrap();
    }

    #[ntex::test]
    async fn without_alpn_and_sni() {
        let (client, server) = pair();
        let (client, server) = join(
            connect(
                Io::new(client, SharedCfg::new("CLI")),
                ssl::Ssl::new(connector(false).context()).unwrap(),
            ),
            handshake(
                Io::new(server, SharedCfg::new("SRV")),
                ssl::Ssl::new(acceptor(false).context()).unwrap(),
                true,
            ),
        )
        .await;
        let (client, server) = (client.unwrap(), server.unwrap());
        assert_eq!(
            client.query::<types::HttpProtocol>().as_ref(),
            Some(&types::HttpProtocol::Http1)
        );
        assert!(server.query::<Servername>().as_ref().is_none());
    }

    #[ntex::test]
    async fn shutdown_after_peer_disconnect() {
        let (client, server) = handshake_pair().await;
        // the peer goes away without close_notify
        drop(server);
        assert!(client.recv(&BytesCodec).await.unwrap().is_none());
        client.shutdown().await.unwrap();
    }

    #[ntex::test]
    async fn invalid_data_after_handshake() {
        let (client, server) = pair();
        let peer = server.clone();
        let (client, server) = join(
            connect(
                Io::new(client, SharedCfg::new("CLI")),
                ssl::Ssl::new(connector(false).context()).unwrap(),
            ),
            handshake(
                Io::new(server, SharedCfg::new("SRV")),
                ssl::Ssl::new(acceptor(false).context()).unwrap(),
                true,
            ),
        )
        .await;
        let (client, _server) = (client.unwrap(), server.unwrap());
        peer.write(b"garbage garbage garbage");
        assert!(client.recv(&BytesCodec).await.is_err());
    }

    #[ntex::test]
    async fn handshake_errors() {
        use ntex_service::Pipeline;

        // timeout
        let (_client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis(50))));
        let err = Pipeline::new((), SslAcceptor::new(acceptor(false)))
            .call(io)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        // peer disconnects
        let (client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV"));
        let ssl = ssl::Ssl::new(acceptor(false).context()).unwrap();
        let (res, ()) = join(handshake(io, ssl, true), client.close()).await;
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);

        // invalid data
        let (client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV"));
        client.write(b"GET / HTTP/1.1\r\n\r\n");
        let ssl = ssl::Ssl::new(acceptor(false).context()).unwrap();
        assert!(handshake(io, ssl, true).await.is_err());

        // the error is reported by the connector
        let (client, server) = pair();
        let io = Io::new(client, SharedCfg::new("CLI"));
        let cfg = SharedCfg::new("CLI").build();
        let (res, ()) = join(
            SslConnector::<ntex_net::connect::Connector<&str>>::new(connector(false)).connect(
                io,
                "localhost",
                &cfg,
            ),
            server.close(),
        )
        .await;
        assert!(res.is_err());
    }

    #[ntex::test]
    async fn acceptor_waits_for_capacity() {
        use ntex_service::Pipeline;
        use ntex_util::future::lazy;

        crate::MAX_SSL_ACCEPT_COUNTER.with(|c| c.set_capacity(1));
        let acceptor = Pipeline::new((), SslAcceptor::new(acceptor(false)));

        let (client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis(30_000))));
        let acceptor2 = acceptor.bind();
        let hnd = ntex::rt::spawn(async move { acceptor2.call(io).await });

        // wait until the handshake holds the only slot
        let mut n = 0;
        while lazy(|cx| acceptor.poll_ready(cx)).await.is_ready() {
            n += 1;
            assert!(n < 1000, "handshake did not start");
            ntex_util::time::sleep(Millis(1)).await;
        }
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_pending());

        // capacity is released by the failed handshake
        client.close().await;
        assert!(hnd.await.unwrap().is_err());
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_ready());
        crate::MAX_SSL_ACCEPT_COUNTER.with(|c| c.set_capacity(256));
    }

    /// Output written while a handshake is in progress must not be lost.
    #[ntex::test]
    async fn write_during_handshake_is_not_lost() {
        let mut acceptor = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor
            .set_private_key(&PKey::private_key_from_pem(KEY).unwrap())
            .unwrap();
        acceptor
            .set_certificate(&X509::from_pem(CERT).unwrap())
            .unwrap();
        let acceptor = acceptor.build();
        let mut connector = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);
        let connector = connector.build();

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1 << 20);
        server.remote_buffer_cap(1 << 20);
        let server = Io::new(server, SharedCfg::new("SRV"));
        let client = Io::new(client, SharedCfg::new("CLI"));

        // the handshake is not driven, `ssl_write` starts it and reports
        // WANT_READ
        let mut ssl = connector
            .configure()
            .unwrap()
            .into_ssl("localhost")
            .unwrap();
        ssl.set_connect_state();
        let stream = new_stream(&client, ssl).unwrap();
        let client = client.add_filter(SslFilter::new(stream));
        client.encode_slice(b"hello").unwrap();

        // reading completes the client handshake
        ntex::rt::spawn(async move {
            let _ = client.recv(&BytesCodec).await;
        });

        let server = handshake(server, ssl::Ssl::new(acceptor.context()).unwrap(), true)
            .await
            .unwrap();
        let item = ntex_util::time::timeout(Millis(1000), server.recv(&BytesCodec))
            .await
            .expect("write is lost")
            .unwrap()
            .unwrap();
        assert_eq!(&item[..], b"hello");
    }

    /// A drained ciphertext buffer goes back to the read buffer cache, a
    /// dropped one would cost an allocation for every read.
    #[ntex::test]
    async fn drained_source_is_cached() {
        let (client, server) = handshake_pair().await;

        // the transport reads into the top of the cache, decrypted data
        // goes to the next buffer
        let page = server.cfg().read_size_min();
        let get = || BytesMut::with_page_size(page);
        let (x, y, p) = (get(), get(), get());
        let src = p.as_ptr();
        drop((x, y, p));

        client
            .send(Bytes::from_static(b"hello"), &BytesCodec)
            .await
            .unwrap();
        let item = server.recv(&BytesCodec).await.unwrap().unwrap();
        assert_eq!(&item[..], b"hello");

        // the decrypted data buffer is released once decoded, after the source
        let top = [get(), get()];
        assert!(
            top.iter().any(|b| b.as_ptr() == src),
            "drained source is not cached"
        );
    }

    /// Buffered output must not look like output produced by reading, that
    /// would pause reads while the write buffer is full.
    #[ntex::test]
    async fn read_is_not_paused_by_buffered_output() {
        let mut acceptor = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor
            .set_private_key(&PKey::private_key_from_pem(KEY).unwrap())
            .unwrap();
        acceptor
            .set_certificate(&X509::from_pem(CERT).unwrap())
            .unwrap();
        let acceptor = acceptor.build();
        let mut connector = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);
        let connector = connector.build();

        let (client, server) = IoTest::create();
        client.remote_buffer_cap(1 << 20);
        server.remote_buffer_cap(1 << 20);
        let peer = client.clone();

        let server = Io::new(
            server,
            SharedCfg::new("SRV").add(IoConfig::default().set_write_backpressure(64)),
        );
        let client = Io::new(client, SharedCfg::new("CLI"));
        let (server, client) = join(
            handshake(server, ssl::Ssl::new(acceptor.context()).unwrap(), true),
            connect(
                client,
                connector
                    .configure()
                    .unwrap()
                    .into_ssl("localhost")
                    .unwrap(),
            ),
        )
        .await;
        let (server, client) = (server.unwrap(), client.unwrap());

        // the peer does not read, the output stays buffered above the high
        // watermark
        peer.remote_buffer_cap(0);
        server.encode_slice(&[b'a'; 256]).unwrap();
        // the failed write moves the output to the page list, more output
        // leaves a partial current page
        ntex_util::time::sleep(Millis(50)).await;
        server.encode_slice(b"b").unwrap();

        // `recv()` waits for the output to drain under write backpressure,
        // and `read_more()` would lift a read pause, so the input is decoded
        // as the read task delivers it
        for msg in [&b"hello"[..], b"world", b"again"] {
            client
                .send(Bytes::copy_from_slice(msg), &BytesCodec)
                .await
                .unwrap();
            let mut item = None;
            for _ in 0..100 {
                item = server.decode(&BytesCodec).unwrap();
                if item.is_some() {
                    break;
                }
                ntex_util::time::sleep(Millis(10)).await;
            }
            assert_eq!(&item.expect("read is paused")[..], msg);
        }
    }

    fn stream_pair() -> (SslStream<IoInner>, SslStream<IoInner>) {
        let stream = |ssl| {
            let inner = IoInner {
                source: None,
                destination: BytePages::new(ntex_bytes::BytePageSize::Size16),
            };
            SslStream::new(ssl, inner).unwrap()
        };
        let mut ssl = connector(false)
            .configure()
            .unwrap()
            .into_ssl("localhost")
            .unwrap();
        ssl.set_connect_state();
        let mut client = stream(ssl);
        let mut ssl = ssl::Ssl::new(acceptor(false).context()).unwrap();
        ssl.set_accept_state();
        let mut server = stream(ssl);

        for _ in 0..10 {
            let _ = client.do_handshake();
            transfer(&mut client, &mut server);
            let _ = server.do_handshake();
            transfer(&mut server, &mut client);
        }
        assert!(client.ssl().is_init_finished() && server.ssl().is_init_finished());
        (client, server)
    }

    fn transfer(from: &mut SslStream<IoInner>, to: &mut SslStream<IoInner>) -> Bytes {
        let data = from.get_mut().destination.freeze();
        let src = to.get_mut().source.get_or_insert_with(BytesMut::new);
        src.extend_from_slice(&data);
        data
    }

    #[allow(clippy::assert_is_empty)]
    fn records(mut data: &[u8]) -> Vec<usize> {
        let mut records = Vec::new();
        while data.len() >= 5 {
            let len = usize::from(u16::from_be_bytes([data[3], data[4]]));
            records.push(len);
            data = &data[5 + len..];
        }
        assert!(data.is_empty());
        records
    }

    /// Small pages are joined with following pages into one record.
    #[test]
    fn write_gathers_pages_into_records() {
        let (mut client, mut server) = stream_pair();

        for (sizes, expected) in [
            (&[300, 8192][..], 1),
            (&[300, 16384][..], 2),
            (&[100, 5000, 6000, 7000][..], 2),
            (&[16000, 300, 84][..], 1),
            (&[20000][..], 2),
        ] {
            let parts: Vec<_> = (0u8..)
                .zip(sizes)
                .map(|(i, &n)| Bytes::from(vec![i; n]))
                .collect();
            let mut src = BytePages::new(ntex_bytes::BytePageSize::Size16);
            for p in &parts {
                src.append(p.clone());
            }
            assert_eq!(src.num_pages(), parts.len());

            write_pages(&mut client, &mut src).unwrap();
            assert!(src.is_empty());
            let wire = transfer(&mut client, &mut server);
            assert_eq!(records(&wire).len(), expected, "{sizes:?}");

            let mut plain = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = server.ssl_read(&mut buf) {
                plain.extend_from_slice(&buf[..n]);
            }
            let expected_plain: Vec<u8> = parts.iter().flat_map(|p| p.iter().copied()).collect();
            assert_eq!(plain, expected_plain, "{sizes:?}");
        }
    }
}
