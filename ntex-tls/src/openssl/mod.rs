//! TLS services and I/O filters backed by OpenSSL.
//!
//! [`SslAcceptor`] and [`SslConnector`] apply [`crate::TlsConfig`] to service
//! handshakes. The free [`accept()`] and [`connect()`] functions instead accept a
//! prepared OpenSSL session and apply neither timeout nor concurrency limiting.
//! Certificates, verification, protocol versions, and ALPN follow the supplied
//! OpenSSL configuration.
//!
//! Filters expose native [`PeerCert`] and [`PeerCertChain`] values as well as
//! backend-independent DER certificates, SNI, PSK identity, TLS version, and
//! cipher metadata when available. [`ntex_io::types::HttpProtocol`] reports
//! HTTP/2 only for negotiated `h2`, otherwise HTTP/1.
use std::{any, borrow::ToOwned, cell::Cell, cell::RefCell, cmp, io, ptr, task::Poll};

use foreign_types_shared::ForeignType;
use ntex_bytes::{BufMut, BytePages, Bytes, BytesMut};
use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer, types};
use openssl_sys as ffi;
use tls_openssl::ssl::{self, NameType, SslStream};
use tls_openssl::x509::X509;

use crate::{PeerCertChainDer, PeerCertDer, PskIdentity, Servername, TlsCipher, TlsVersion};

mod connect;
pub use self::connect::SslConnector;

mod accept;
pub use self::accept::SslAcceptor;

mod alloc;
pub use self::alloc::use_global_allocator;

/// Configures a fresh OpenSSL context with a portable identity and ordered intermediates.
///
/// This does not change peer verification policy or include the self-signed root.
///
/// # Errors
/// Native non-exportable keys are unsupported. Other errors report chain ordering,
/// key/certificate decoding, or OpenSSL configuration failures.
pub fn set_identity(
    context: &mut ssl::SslContextBuilder,
    identity: &crate::x509::Identity,
) -> Result<(), crate::x509::Error> {
    use crate::x509::Error;
    let invalid = |e: tls_openssl::error::ErrorStack| Error::InvalidIdentity(e.to_string());
    let key =
        tls_openssl::pkey::PKey::private_key_from_pkcs8(identity.private_key().export_pkcs8_der()?)
            .map_err(invalid)?;
    let cert = X509::from_der(identity.cert().der()).map_err(invalid)?;
    let chain = identity
        .ordered_chain(false)?
        .iter()
        .map(|cert| X509::from_der(cert.der()).map_err(invalid))
        .collect::<Result<Vec<_>, _>>()?;
    context.set_certificate(&cert).map_err(invalid)?;
    context.set_private_key(&key).map_err(invalid)?;
    for cert in chain {
        context.add_extra_chain_cert(cert).map_err(invalid)?;
    }
    context.check_private_key().map_err(invalid)
}

/// The peer's end-entity certificate as an owned OpenSSL reference.
///
/// The query returns no value if the peer did not supply a certificate.
#[derive(Debug)]
pub struct PeerCert(
    /// Reference-counted OpenSSL certificate.
    pub X509,
);

/// The peer certificate chain with OpenSSL's native chain semantics.
///
/// On a client connection the chain includes the server's leaf; on a server
/// connection OpenSSL omits the client's leaf. Query [`PeerCertChainDer`] for a
/// backend-independent, leaf-first chain instead. Reading either chain does not
/// change certificate verification policy.
#[derive(Debug)]
pub struct PeerCertChain(
    /// Owned references to the certificates reported by OpenSSL.
    pub Vec<X509>,
);

/// Encrypts and decrypts either side of an OpenSSL TLS connection.
///
/// Created by the handshake functions or services in this module. The filter
/// preserves plaintext that OpenSSL cannot yet accept, including during
/// negotiation, and exchanges `close_notify` during shutdown.
#[derive(Debug)]
pub struct SslFilter {
    /// Stream accessed only within non-reentrant, single-threaded filter operations.
    inner: RefCell<SslStream<IoInner>>,
}

/// In-memory transport adapter used by OpenSSL's BIO callbacks.
///
/// Empty input reports `WouldBlock`, not EOF. Output is queued into pages for
/// the lower I/O filter; flushing this adapter does not flush the transport.
#[derive(Debug)]
struct IoInner {
    /// Ciphertext temporarily borrowed from the lower filter's read buffer.
    source: Option<BytesMut>,
    /// Ciphertext produced by OpenSSL and returned to the lower write buffer.
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
    /// Wraps an initialized stream without starting negotiation.
    fn new(stream: SslStream<IoInner>) -> Self {
        Self {
            inner: RefCell::new(stream),
        }
    }

    /// Reads the negotiated IANA cipher identifier and optional standard name.
    #[cfg(ntex_openssl_cipher)]
    fn cipher(ssl: &ssl::SslRef) -> Option<TlsCipher> {
        let cipher = ssl.current_cipher()?;
        Some(TlsCipher {
            id: u16::from_be_bytes(cipher.protocol_id()),
            name: cipher.standard_name(),
        })
    }

    /// Leaves the cipher query unavailable for libraries without the required API.
    #[cfg(not(ntex_openssl_cipher))]
    fn cipher(_: &ssl::SslRef) -> Option<TlsCipher> {
        None
    }

    /// Runs a stream operation while reusing the lower filter's current write page.
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

    /// Transfers ciphertext buffers into the adapter for one exclusive stream operation.
    ///
    /// Returns unread input, including empty reusable storage, and generated
    /// output afterward. `f` must not reenter this filter while it borrows the
    /// stream; notifications that can do so must run after this method returns.
    fn with_buffers_inner<F, R>(&self, buf: &FilterBuf<'_>, reuse_page: bool, f: F) -> R
    where
        F: FnOnce(&mut SslStream<IoInner>, &FilterBuf<'_>) -> R,
    {
        #[cfg(test)]
        let _borrow = crate::utils::testing::TlsBorrow::new();

        // Neither the BIO callbacks nor the buffer operations below reenter
        // the filter. User handshake callbacks may only query it, which
        // returns `None` while the stream is borrowed.
        let mut stream = self.inner.borrow_mut();
        let stream = &mut *stream;

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
    /// Produces owned metadata, normalizing the DER chain to include the peer leaf.
    ///
    /// Returns `None` while the stream is busy, for example when a verify or
    /// ALPN callback queries its own connection during the handshake.
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        let stream = self.inner.try_borrow().ok()?;
        let ssl = stream.ssl();
        if id == any::TypeId::of::<types::HttpProtocol>() {
            let alpn = ssl.selected_alpn_protocol();
            Some(Box::new(crate::utils::http_protocol(alpn)))
        } else if id == any::TypeId::of::<PeerCert>() {
            Some(Box::new(PeerCert(ssl.peer_certificate()?)))
        } else if id == any::TypeId::of::<PeerCertChain>() {
            let chain = ssl.peer_cert_chain()?;
            Some(Box::new(PeerCertChain(
                chain.iter().map(ToOwned::to_owned).collect(),
            )))
        } else if id == any::TypeId::of::<PeerCertDer>() {
            let cert = ssl.peer_certificate()?;
            Some(Box::new(PeerCertDer(Bytes::from(cert.to_der().ok()?))))
        } else if id == any::TypeId::of::<PeerCertChainDer>() {
            let mut chain = Vec::new();
            for cert in ssl.peer_cert_chain().into_iter().flatten() {
                chain.push(Bytes::from(cert.to_der().ok()?));
            }
            // the chain does not include the client's certificate on the
            // server side
            if let Some(cert) = ssl.peer_certificate() {
                let cert = Bytes::from(cert.to_der().ok()?);
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
            let name = ssl.servername(NameType::HOST_NAME)?;
            Some(Box::new(Servername(name.to_string())))
        } else if id == any::TypeId::of::<PskIdentity>() {
            let psk_id = ssl.psk_identity()?;
            Some(Box::new(PskIdentity(Bytes::copy_from_slice(psk_id))))
        } else if id == any::TypeId::of::<TlsVersion>() {
            let version = match ssl.version2()? {
                ssl::SslVersion::TLS1 => TlsVersion::Tls10,
                ssl::SslVersion::TLS1_1 => TlsVersion::Tls11,
                ssl::SslVersion::TLS1_2 => TlsVersion::Tls12,
                ssl::SslVersion::TLS1_3 => TlsVersion::Tls13,
                _ => return None,
            };
            Some(Box::new(version))
        } else if id == any::TypeId::of::<TlsCipher>() {
            Some(Box::new(Self::cipher(ssl)?))
        } else {
            None
        }
    }

    /// Sends `close_notify` and waits for the peer's alert or transport EOF.
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

    /// Drains decrypted data and closes I/O only after releasing the mutable TLS stream.
    fn process_read_buf(&self, rb: &FilterBuf<'_>) -> io::Result<()> {
        let mut peer_closed = false;
        let result = self.with_read_buffers(rb, |stream, buf| {
            buf.with_read_buffers(|_, dst| {
                loop {
                    if dst.remaining_mut() == 0 {
                        dst.reserve_more();
                    }

                    // SAFETY: `ssl_read_uninit` only writes decrypted, initialized bytes.
                    let chunk = unsafe { dst.chunk_mut().as_uninit_slice_mut() };
                    match stream.ssl_read_uninit(chunk) {
                        // SAFETY: `ssl_read_uninit` initialized the first `v` bytes of `chunk`.
                        Ok(v) => unsafe { dst.advance_mut(v) },
                        Err(e) => {
                            return match e.code() {
                                ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE => Ok(()),
                                ssl::ErrorCode::ZERO_RETURN => {
                                    peer_closed = true;
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
        });
        // Closing can reenter the filter through an inline read-task waker.
        if peer_closed {
            rb.io().close();
        }
        result
    }

    /// Encrypts queued pages, retaining plaintext that needs a later retry.
    fn process_write_buf(&self, wb: &FilterBuf<'_>) -> io::Result<()> {
        self.with_buffers(wb, |stream, buf| {
            buf.with_write_buffers(|w_src, _| write_pages(stream, w_src))
        })
    }
}

/// Maximum plaintext size of a TLS record.
const MAX_RECORD: usize = 16 * 1024;

thread_local! {
    /// Buffer that joins pages into a record, shared by the connections of a thread.
    static STAGING: Cell<Vec<u8>> = const { Cell::new(Vec::new()) };
}

/// Encrypts plaintext pages, gathering small adjacent pages into full-sized records.
///
/// `WANT_READ` and `WANT_WRITE` consume no input. Gathered bytes needed for a
/// retry are copied back into `src` before the shared staging storage is reused.
fn write_pages(stream: &mut SslStream<IoInner>, src: &mut BytePages) -> io::Result<()> {
    let mut staging = Staging(None);
    while let Some(mut page) = src.take() {
        let staged = staging.gather(&page, src);
        let (written, retry) = match stream.ssl_write(staged.unwrap_or(&page)) {
            Ok(v) => (v, false),
            Err(e)
                if matches!(
                    e.code(),
                    ssl::ErrorCode::WANT_READ | ssl::ErrorCode::WANT_WRITE
                ) =>
            {
                // nothing is consumed, e.g. a handshake is in
                // progress, the write is retried later
                (0, true)
            }
            Err(e) => return Err(io::Error::other(e)),
        };
        if let Some(buf) = staged {
            // the staging buffer is reused, a retry needs the same bytes
            if written < buf.len() {
                src.prepend(BytesMut::copy_from_slice(&buf[written..]));
            }
        } else {
            page.advance_to(written);
            src.prepend(page);
        }
        if retry {
            break;
        }
    }
    Ok(())
}

/// The thread's staging buffer, taken out of it while a write uses it.
///
/// A nested write, e.g. from an OpenSSL callback, allocates a buffer of its own.
struct Staging(Option<Vec<u8>>);

impl Staging {
    /// Joins `page` with the following whole pages that fit into one record,
    /// returns `None` if the next page does not fit.
    ///
    /// Every `ssl_write` call produces its own record, so a small page, e.g.
    /// response headers in front of a body, would otherwise cost a record.
    fn gather(&mut self, page: &[u8], src: &mut BytePages) -> Option<&[u8]> {
        let next = src.take()?;
        if page.len() + next.len() > MAX_RECORD {
            src.prepend(next);
            return None;
        }
        let buf = self
            .0
            .get_or_insert_with(|| STAGING.try_with(Cell::take).unwrap_or_default());
        buf.clear();
        buf.reserve_exact(MAX_RECORD);
        buf.extend_from_slice(page);
        buf.extend_from_slice(&next);
        while let Some(next) = src.take() {
            if buf.len() + next.len() > MAX_RECORD {
                src.prepend(next);
                break;
            }
            buf.extend_from_slice(&next);
        }
        Some(buf)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Some(buf) = self.0.take() {
            let _ = STAGING.try_with(|staging| staging.set(buf));
        }
    }
}

/// Creates the BIO-backed stream with read-ahead and the I/O-configured output page size.
fn new_stream<F>(io: &Io<F>, ssl: ssl::Ssl) -> io::Result<SslStream<IoInner>> {
    // Let OpenSSL pull all buffered ciphertext in one BIO read, instead of
    // reading every record header and body separately.
    // SAFETY: `ssl` owns a live `SSL`, this control takes no pointer argument.
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

/// Installs a TLS filter and completes a server handshake.
///
/// Unlike [`SslAcceptor`], the caller provides a prepared `Ssl`, so per
/// connection settings such as the certificate can be selected before the
/// handshake. No handshake timeout or concurrency limit is applied.
/// Existing I/O filters remain below TLS.
///
/// # Errors
///
/// Returns OpenSSL setup or negotiation errors, transport errors, or
/// [`io::ErrorKind::UnexpectedEof`] if the peer disconnects before completion.
pub async fn accept<F: Filter>(
    io: Io<F>,
    ssl: ssl::Ssl,
) -> Result<Io<Layer<SslFilter, F>>, io::Error> {
    handshake(io, ssl, true).await
}

/// Installs a TLS filter and completes a client handshake.
///
/// The caller must prepare `ssl`, including its peer-verification and server-name
/// settings. Unlike [`SslConnector::connect`], this function does not derive
/// those settings from a host or apply [`crate::TlsConfig`].
/// No handshake timeout or concurrency limit is applied. Existing filters remain
/// below TLS.
///
/// # Errors
///
/// Returns OpenSSL setup or negotiation errors, transport errors, or
/// [`io::ErrorKind::UnexpectedEof`] if the peer disconnects before completion.
pub async fn connect<F: Filter>(
    io: Io<F>,
    ssl: ssl::Ssl,
) -> Result<Io<Layer<SslFilter, F>>, io::Error> {
    handshake(io, ssl, false).await
}

/// Selects the TLS role, installs the filter, and drives OpenSSL negotiation.
///
/// Read notifications may carry both final handshake bytes and EOF, so EOF
/// becomes an error only if another handshake step still needs input.
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

    /// STATEMENT: Handshake callbacks that query their own connection get `None`
    /// instead of aliasing the borrowed stream.
    ///
    /// HOW:
    /// - Install a client verify callback and a server SNI callback that query
    ///   `HttpProtocol` on their own `IoRef` while the stream is busy.
    /// - Check both callbacks ran and every query they made was unanswered.
    /// - Check the same query is answered on both sides after the handshake.
    #[ntex::test]
    async fn handshake_callbacks_query_own_io() {
        use crate::utils::testing;

        let (client, server) = pair();
        let client = Io::new(client, SharedCfg::new("CLI"));
        let server = Io::new(server, SharedCfg::new("SRV"));
        let cli = testing::callback_io(client.get_ref());
        let srv = testing::callback_io(server.get_ref());

        let mut connector = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify_callback(SslVerifyMode::PEER, move |_, _| {
            testing::query_from_callback(cli);
            true
        });
        let mut acceptor = ssl::SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor
            .set_private_key(&PKey::private_key_from_pem(KEY).unwrap())
            .unwrap();
        acceptor
            .set_certificate(&X509::from_pem(CERT).unwrap())
            .unwrap();
        acceptor.set_servername_callback(move |_, _| {
            testing::query_from_callback(srv);
            Ok(())
        });

        let (client, server) = join(
            connect(
                client,
                connector
                    .build()
                    .configure()
                    .unwrap()
                    .into_ssl("localhost")
                    .unwrap(),
            ),
            accept(server, ssl::Ssl::new(acceptor.build().context()).unwrap()),
        )
        .await;
        let (client, server) = (client.unwrap(), server.unwrap());

        let queries = testing::callback_queries();
        assert!(queries.contains(&(cli, false)), "{queries:?}");
        assert!(queries.contains(&(srv, false)), "{queries:?}");
        assert!(queries.iter().all(|(_, answered)| !answered), "{queries:?}");
        assert!(client.query::<types::HttpProtocol>().as_ref().is_some());
        assert!(server.query::<types::HttpProtocol>().as_ref().is_some());
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
                    connect(
                        client,
                        connector(false)
                            .configure()
                            .unwrap()
                            .into_ssl("localhost")
                            .unwrap(),
                    ),
                    handshake(
                        server,
                        ssl::Ssl::new(acceptor(false).context()).unwrap(),
                        true,
                    ),
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

    /// STATEMENT: DER certificate queries return the peer's leaf first on both sides.
    ///
    /// HOW:
    /// - Generate a certificate chain and present its leaf and intermediate from both peers.
    /// - Compare each peer's `PeerCertDer` and `PeerCertChainDer` with the original DER bytes.
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
            let cert = io.query::<PeerCertDer>().into_inner().unwrap();
            assert_eq!(cert.0, der[0][..]);
            let chain = io.query::<PeerCertChainDer>().into_inner().unwrap();
            assert_eq!(chain.0, der);
        }
    }

    /// STATEMENT: `accept` uses the supplied `Ssl` configuration and exposes negotiated metadata.
    ///
    /// HOW:
    /// - Supply a server `Ssl` with a generated certificate and request a different client certificate.
    /// - Check both peer certificates, server SNI, TLS 1.3, and matching cipher metadata when supported.
    #[ntex::test]
    async fn accept_with_ssl() {
        let [leaf, intermediate, _] = cert_chain();

        let mut acceptor = ssl::SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        acceptor.set_private_key(&leaf.1).unwrap();
        acceptor.set_certificate(&leaf.0).unwrap();
        acceptor.set_verify_callback(SslVerifyMode::PEER, |_, _| true);
        let acceptor = acceptor.build();

        let mut connector = ssl::SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);
        connector.set_private_key(&intermediate.1).unwrap();
        connector.set_certificate(&intermediate.0).unwrap();
        let ssl = connector
            .build()
            .configure()
            .unwrap()
            .into_ssl("localhost")
            .unwrap();

        let (client, server) = pair();
        let (client, server) = join(
            connect(Io::new(client, SharedCfg::new("CLI")), ssl),
            accept(
                Io::new(server, SharedCfg::new("SRV")),
                ssl::Ssl::new(acceptor.context()).unwrap(),
            ),
        )
        .await;
        let (client, server) = (client.unwrap(), server.unwrap());

        let cert = |c: &X509| c.to_der().unwrap();
        assert_eq!(
            client.query::<PeerCert>().as_ref().map(|c| cert(&c.0)),
            Some(cert(&leaf.0))
        );
        assert_eq!(
            server.query::<PeerCert>().as_ref().map(|c| cert(&c.0)),
            Some(cert(&intermediate.0))
        );
        assert_eq!(
            server.query::<Servername>().as_ref().map(|s| s.0.as_str()),
            Some("localhost")
        );

        for io in [&client, &server] {
            assert_eq!(io.query::<TlsVersion>().as_ref(), Some(&TlsVersion::Tls13));
        }
        #[cfg(ntex_openssl_cipher)]
        {
            let cipher = *client.query::<TlsCipher>().as_ref().unwrap();
            assert_eq!(server.query::<TlsCipher>().as_ref(), Some(&cipher));
            // TLS 1.3 suites are 0x13XX
            assert_eq!(cipher.id >> 8, 0x13);
            assert!(cipher.name.unwrap().starts_with("TLS_AES_"));
        }
    }

    /// STATEMENT: TLS 1.2 queries report the negotiated version and IANA cipher names.
    ///
    /// HOW:
    /// - Handshake with the TLS 1.2 configuration and check both peers' version queries.
    /// - When cipher queries are supported, compare their IDs and validate the IANA name.
    #[ntex::test]
    async fn tls12_version_and_cipher() {
        let (client, server) = handshake_pair().await;
        for io in [&client, &server] {
            assert_eq!(io.query::<TlsVersion>().as_ref(), Some(&TlsVersion::Tls12));
        }
        #[cfg(ntex_openssl_cipher)]
        {
            let cipher = *client.query::<TlsCipher>().as_ref().unwrap();
            assert_eq!(server.query::<TlsCipher>().as_ref(), Some(&cipher));
            // tls 1.2 names are IANA names, not openssl names
            let name = cipher.name.unwrap();
            assert!(name.starts_with("TLS_ECDHE_RSA_WITH_"));
            assert_ne!(ssl::cipher_name(name), "(NONE)");
            assert_ne!(cipher.id >> 8, 0x13);
        }
    }

    /// STATEMENT: OpenSSL services expose negotiated metadata and carry data through clean shutdown.
    ///
    /// HOW:
    /// - Connect in-memory services with `h2` and check ALPN, SNI, the server certificate,
    ///   and absent PSK identity, client certificates, and unsupported query results.
    /// - Receive 256 KiB on the server and verify client shutdown yields peer EOF.
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

    /// STATEMENT: Connections without ALPN or SNI report HTTP/1 and no server name.
    ///
    /// HOW:
    /// - Handshake using a client `Ssl` without ALPN or a configured host name.
    /// - Check the client's HTTP protocol and the absence of the server's `Servername`.
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

    /// STATEMENT: Shutdown succeeds after a peer disconnects without sending `close_notify`.
    ///
    /// HOW:
    /// - Complete a handshake, drop the server, and check the client receives EOF.
    /// - Await client shutdown and assert it succeeds.
    #[ntex::test]
    async fn shutdown_after_peer_disconnect() {
        let (client, server) = handshake_pair().await;
        // the peer goes away without close_notify
        drop(server);
        assert!(client.recv(&BytesCodec).await.unwrap().is_none());
        client.shutdown().await.unwrap();
    }

    /// STATEMENT: Invalid TLS records are rejected after a successful handshake.
    ///
    /// HOW:
    /// - Handshake an in-memory pair, then inject plaintext garbage through the raw server transport.
    /// - Check that receiving on the TLS client returns an error.
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

    /// STATEMENT: Connector timeout errors use service configuration rather than I/O configuration.
    ///
    /// HOW:
    /// - Return an idle transport with its TLS timeout disabled from a custom connector.
    /// - Set a 50 ms service timeout and check the bounded call returns `TimedOut` with
    ///   the service's configured diagnostic identity.
    #[ntex::test]
    async fn connector_handshake_timeout() {
        use std::{cell::RefCell, rc::Rc};

        use ntex_error::{Error, ErrorDiagnostic};
        use ntex_net::connect::{Connect, ConnectError, Connector};
        use ntex_service::{Pipeline, fn_service};

        let (client, _server) = pair();
        let client = Rc::new(RefCell::new(Some(Io::new(
            client,
            SharedCfg::new("IO")
                .service("transport")
                .add(tls_cfg(Millis::ZERO)),
        ))));
        let connector = SslConnector::<Connector<&str>>::new(connector(false)).connector(
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

    /// STATEMENT: Handshake failures preserve timeout and disconnect errors and reject non-TLS data.
    ///
    /// HOW:
    /// - Stall an acceptor with a 50 ms timeout and check for `TimedOut` and released capacity.
    /// - Disconnect during server negotiation and check for `UnexpectedEof`; also check
    ///   that plain HTTP input and a disconnected connector peer produce errors.
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
        crate::MAX_SSL_ACCEPT_COUNTER.with(|c| assert_eq!(c.total(), 0));

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

    /// STATEMENT: Acceptor readiness waits for handshake capacity and resumes when it is released.
    ///
    /// HOW:
    /// - Set capacity to one and start a stalled handshake through a bound acceptor.
    /// - Check readiness becomes pending, close the peer, then check failure restores readiness.
    /// - Poll another stalled call, drop it, and check cancellation releases its slot and readiness.
    #[ntex::test]
    async fn acceptor_waits_for_capacity() {
        use std::future::Future;

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

        let (_client, server) = pair();
        let io = Io::new(server, SharedCfg::new("SRV").add(tls_cfg(Millis::ZERO)));
        let mut call = Box::pin(acceptor.call(io));
        assert!(lazy(|cx| call.as_mut().poll(cx)).await.is_pending());
        crate::MAX_SSL_ACCEPT_COUNTER.with(|c| assert_eq!(c.total(), 1));
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_pending());
        drop(call);
        crate::MAX_SSL_ACCEPT_COUNTER.with(|c| assert_eq!(c.total(), 0));
        assert!(lazy(|cx| acceptor.poll_ready(cx)).await.is_ready());
        crate::MAX_SSL_ACCEPT_COUNTER.with(|c| c.set_capacity(256));
    }

    /// STATEMENT: Output queued before the TLS handshake finishes is not lost.
    ///
    /// HOW:
    /// - Encode `hello` before driving the client handshake, then spawn a client read to progress it.
    /// - Complete the server handshake and check it receives `hello` within a bounded wait.
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

    /// STATEMENT: A drained ciphertext buffer returns to the read buffer cache.
    ///
    /// HOW:
    /// - Prime the cache with known page allocations, then send and verify `hello`.
    /// - Allocate two pages again and check one reuses the recorded ciphertext-buffer pointer.
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

    /// STATEMENT: Already-buffered output does not pause incoming TLS reads under write backpressure.
    ///
    /// HOW:
    /// - Stop the peer reading and queue output above a 64-byte write watermark, including a partial page.
    /// - Send three client messages and check direct server decoding receives each without lifting a read pause.
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

    fn raw_stream(ssl: ssl::Ssl) -> SslStream<IoInner> {
        let inner = IoInner {
            source: None,
            destination: BytePages::new(ntex_bytes::BytePageSize::Size16),
        };
        SslStream::new(ssl, inner).unwrap()
    }

    fn stream_handshake(client: &mut SslStream<IoInner>, server: &mut SslStream<IoInner>) {
        for _ in 0..10 {
            let _ = client.do_handshake();
            transfer(client, server);
            let _ = server.do_handshake();
            transfer(server, client);
        }
        assert!(client.ssl().is_init_finished() && server.ssl().is_init_finished());
    }

    fn stream_pair() -> (SslStream<IoInner>, SslStream<IoInner>) {
        let mut ssl = connector(false)
            .configure()
            .unwrap()
            .into_ssl("localhost")
            .unwrap();
        ssl.set_connect_state();
        let mut client = raw_stream(ssl);
        let mut ssl = ssl::Ssl::new(acceptor(false).context()).unwrap();
        ssl.set_accept_state();
        let mut server = raw_stream(ssl);
        stream_handshake(&mut client, &mut server);
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

    /// STATEMENT: Adjacent small pages are gathered into TLS records without changing plaintext.
    ///
    /// HOW:
    /// - Queue varied page sizes and write them through a handshaken stream.
    /// - Check the source drains, count the resulting records, and compare decrypted bytes with the input.
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

    /// STATEMENT: Gathered writes retain their bytes across handshake retries and staging-buffer reuse.
    ///
    /// HOW:
    /// - Queue two pages before the handshake without `SSL_MODE_ACCEPT_MOVING_WRITE_BUFFER`.
    /// - Write different pages on another stream, finish the first handshake, and retry its write.
    /// - Check the first queue drains into one record containing exactly its original bytes.
    #[test]
    fn gathered_write_retried_after_handshake() {
        let mut ctx = ssl::SslContext::builder(SslMethod::tls()).unwrap();
        ctx.set_verify(SslVerifyMode::NONE);
        let mut ssl = ssl::Ssl::new(&ctx.build()).unwrap();
        ssl.set_connect_state();
        let mut client = raw_stream(ssl);
        let mut ssl = ssl::Ssl::new(acceptor(false).context()).unwrap();
        ssl.set_accept_state();
        let mut server = raw_stream(ssl);

        let pages = |sizes: &[(u8, usize)]| {
            let mut src = BytePages::new(ntex_bytes::BytePageSize::Size16);
            for &(b, n) in sizes {
                src.append(Bytes::from(vec![b; n]));
            }
            src
        };
        let mut src = pages(&[(1, 300), (2, 8192)]);
        assert_eq!(src.num_pages(), 2);
        write_pages(&mut client, &mut src).unwrap();
        assert_eq!(src.len(), 8492);

        let (mut other, _) = stream_pair();
        let mut other_src = pages(&[(3, 5000), (4, 5000)]);
        write_pages(&mut other, &mut other_src).unwrap();
        assert!(other_src.is_empty());

        stream_handshake(&mut client, &mut server);
        write_pages(&mut client, &mut src).unwrap();
        assert!(src.is_empty());
        assert_eq!(records(&transfer(&mut client, &mut server)).len(), 1);

        let mut plain = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = server.ssl_read(&mut buf) {
            plain.extend_from_slice(&buf[..n]);
        }
        let mut expected = vec![1; 300];
        expected.resize(8492, 2);
        assert_eq!(plain, expected);
    }
}
