//! An implementation of TLS streams backed by Windows Schannel.
#![cfg(windows)]
use std::sync::{Arc, Mutex, PoisonError};
use std::{any, cell::UnsafeCell, io, mem, ptr, task::Poll};

use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer, types};
use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
use windows_sys::Win32::Foundation::SEC_E_OK;
use windows_sys::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, FreeCredentialsHandle, SCH_CRED_AUTO_CRED_VALIDATION,
    SCH_CRED_MANUAL_CRED_VALIDATION, SCH_CRED_NO_DEFAULT_CREDS, SCH_CRED_NO_SERVERNAME_CHECK,
    SCH_CRED_NO_SYSTEM_MAPPER, SCH_CREDENTIALS, SCH_CREDENTIALS_VERSION, SCH_USE_STRONG_CRYPTO,
    SCHANNEL_CRED, SCHANNEL_CRED_VERSION, SECPKG_CRED_INBOUND, SECPKG_CRED_OUTBOUND,
    SecApplicationProtocolNegotiationExt_ALPN, UNISP_NAME_W,
};
use windows_sys::Win32::Security::Credentials::SecHandle;
use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

use crate::Servername;

mod accept;
mod cert;
mod connect;
mod context;
pub use self::accept::TlsAcceptor;
pub use self::cert::{CertStoreLocation, Certificate};
pub use self::connect::TlsConnector;
use self::context::{Context, Decrypted, HandshakeState};

/// Windows Schannel client configuration.
///
/// Clones share the credentials handle, so connections made with the same
/// configuration can resume TLS sessions.
///
/// Schannel offers a cached session's protocol version only. After a TLS 1.2
/// session with a server, the next connection to the same server name offers
/// TLS 1.2 only and fails if the server no longer supports it. The cache
/// expires after 10 hours by default (`ClientCacheTime`); a new configuration
/// starts with an empty cache.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    verify: bool,
    /// Prebuilt `SECBUFFER_APPLICATION_PROTOCOLS` buffer, `None` disables ALPN.
    alpn: Option<Arc<[u8]>>,
    /// Client certificate, sent when the server requests one
    cert: Option<Certificate>,
    /// Lazily acquired credentials, Schannel caches sessions per credentials handle.
    cred: Arc<Mutex<Option<Arc<Credentials>>>>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientConfig {
    /// Construct default Schannel client configuration.
    ///
    /// ALPN offers `h2` and `http/1.1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            verify: true,
            alpn: alpn_buffer(&[b"h2".as_slice(), b"http/1.1".as_slice()]),
            cert: None,
            cred: Arc::default(),
        }
    }

    /// Accept invalid server certificates and hostnames.
    #[must_use]
    pub fn danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> Self {
        self.verify = !accept_invalid_certs;
        // credentials depend on the validation mode
        self.cred = Arc::default();
        self
    }

    /// Set ALPN protocols offered to the server, in preference order.
    ///
    /// An empty list disables ALPN.
    ///
    /// # Panics
    ///
    /// Panics if a protocol is empty or longer than 255 bytes, or the encoded
    /// list exceeds 65535 bytes.
    #[must_use]
    pub fn set_alpn_protocols<T: AsRef<[u8]>>(mut self, protocols: &[T]) -> Self {
        self.alpn = alpn_buffer(protocols);
        self
    }

    /// Set the client certificate, sent when the server requests one.
    ///
    /// Without it, no certificate is sent.
    #[must_use]
    pub fn set_client_cert(mut self, cert: Certificate) -> Self {
        self.cert = Some(cert);
        // credentials carry the certificate
        self.cred = Arc::default();
        self
    }

    fn credentials(&self) -> io::Result<Arc<Credentials>> {
        let mut cred = self.cred.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cred) = &*cred {
            return Ok(cred.clone());
        }
        let new = Arc::new(Credentials::acquire(self.verify, self.cert.as_ref())?);
        *cred = Some(new.clone());
        Ok(new)
    }
}

/// Windows Schannel server configuration.
///
/// Clones share the credentials handle, so Schannel can resume TLS sessions
/// of connections accepted with the same configuration.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Prebuilt `SECBUFFER_APPLICATION_PROTOCOLS` buffer, `None` disables ALPN.
    alpn: Option<Arc<[u8]>>,
    request_client_cert: bool,
    cred: Arc<Credentials>,
}

impl ServerConfig {
    /// Construct Schannel server configuration with the server certificate.
    ///
    /// ALPN accepts `h2` and `http/1.1`.
    ///
    /// # Errors
    ///
    /// Fails if Schannel rejects the certificate, for example if its private
    /// key is not accessible.
    pub fn new(cert: Certificate) -> io::Result<Self> {
        Ok(Self {
            alpn: alpn_buffer(&[b"h2".as_slice(), b"http/1.1".as_slice()]),
            request_client_cert: false,
            cred: Arc::new(Credentials::inbound(cert)?),
        })
    }

    /// Set ALPN protocols accepted from clients, in preference order.
    ///
    /// An empty list disables ALPN.
    ///
    /// # Panics
    ///
    /// Panics if a protocol is empty or longer than 255 bytes, or the encoded
    /// list exceeds 65535 bytes.
    #[must_use]
    pub fn set_alpn_protocols<T: AsRef<[u8]>>(mut self, protocols: &[T]) -> Self {
        self.alpn = alpn_buffer(protocols);
        self
    }

    /// Request a certificate from clients.
    ///
    /// The handshake completes without one if the client does not send it.
    /// A certificate sent by the client is available as [`PeerCert`].
    ///
    /// Schannel checks that the client owns the certificate's private key,
    /// but does not validate the certificate, the application must check it.
    #[must_use]
    pub fn request_client_cert(mut self, request: bool) -> Self {
        self.request_client_cert = request;
        self
    }
}

/// Schannel credentials handle.
struct Credentials {
    handle: SecHandle,
    /// Keeps the certificate alive
    _cert: Option<Certificate>,
}

// Schannel credentials handles can be used from multiple threads.
unsafe impl Send for Credentials {}
unsafe impl Sync for Credentials {}

impl Credentials {
    /// Acquires client credentials.
    fn acquire(verify: bool, cert: Option<&Certificate>) -> io::Result<Self> {
        // never pick a client certificate from the user's store on its own
        let mut flags = SCH_USE_STRONG_CRYPTO | SCH_CRED_NO_DEFAULT_CREDS;
        if verify {
            flags |= SCH_CRED_AUTO_CRED_VALIDATION;
        } else {
            flags |= SCH_CRED_MANUAL_CRED_VALIDATION | SCH_CRED_NO_SERVERNAME_CHECK;
        }
        let handle = Self::with_flags(SECPKG_CRED_OUTBOUND, flags, cert)?;
        Ok(Self {
            handle,
            _cert: cert.cloned(),
        })
    }

    /// Acquires server credentials.
    fn inbound(cert: Certificate) -> io::Result<Self> {
        // do not map client certificates to Windows accounts
        let flags = SCH_USE_STRONG_CRYPTO | SCH_CRED_NO_SYSTEM_MAPPER;
        let handle = Self::with_flags(SECPKG_CRED_INBOUND, flags, Some(&cert))?;
        Ok(Self {
            handle,
            _cert: Some(cert),
        })
    }

    fn with_flags(direction: u32, flags: u32, cert: Option<&Certificate>) -> io::Result<SecHandle> {
        // Schannel keeps its own reference to the certificate
        let mut certs = [cert.map_or(ptr::null_mut(), |cert| cert.as_ptr().cast_mut())];
        let (num_certs, certs) = if cert.is_some() {
            (1, certs.as_mut_ptr())
        } else {
            (0, ptr::null_mut())
        };

        if supports_sch_credentials() {
            // SCH_CREDENTIALS enables the system default protocols, including TLS 1.3
            let mut sch_cred = unsafe { mem::zeroed::<SCH_CREDENTIALS>() };
            sch_cred.dwVersion = SCH_CREDENTIALS_VERSION;
            sch_cred.dwFlags = flags;
            sch_cred.cCreds = num_certs;
            sch_cred.paCred = certs;
            Self::acquire_with(direction, (&raw mut sch_cred).cast())
        } else {
            let mut schannel_cred = unsafe { mem::zeroed::<SCHANNEL_CRED>() };
            schannel_cred.dwVersion = SCHANNEL_CRED_VERSION;
            schannel_cred.dwFlags = flags;
            schannel_cred.cCreds = num_certs;
            schannel_cred.paCred = certs;
            Self::acquire_with(direction, (&raw mut schannel_cred).cast())
        }
    }

    fn acquire_with(direction: u32, auth_data: *mut std::ffi::c_void) -> io::Result<SecHandle> {
        let mut cred = unsafe { mem::zeroed::<SecHandle>() };
        let mut expiry = 0i64;
        let status = unsafe {
            AcquireCredentialsHandleW(
                ptr::null(),
                UNISP_NAME_W,
                direction,
                ptr::null(),
                auth_data,
                None,
                ptr::null(),
                &raw mut cred,
                &raw mut expiry,
            )
        };
        if status == SEC_E_OK {
            Ok(cred)
        } else {
            Err(sspi_error("AcquireCredentialsHandleW", status))
        }
    }
}

/// `SCH_CREDENTIALS` is supported since Windows 10 1809, earlier versions
/// support `SCHANNEL_CRED` only.
///
/// Errors of `SCH_CREDENTIALS` are reported as is, a fallback to
/// `SCHANNEL_CRED` would hide them and offer older protocols only.
fn supports_sch_credentials() -> bool {
    let mut info = unsafe { mem::zeroed::<OSVERSIONINFOW>() };
    #[allow(clippy::cast_possible_truncation)]
    {
        info.dwOSVersionInfoSize = u32::try_from(mem::size_of::<OSVERSIONINFOW>()).unwrap_or(0);
    }
    // unlike GetVersionExW, not affected by the application manifest
    unsafe { RtlGetVersion(&raw mut info) };
    (info.dwMajorVersion, info.dwBuildNumber) >= (10, 17763)
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").finish_non_exhaustive()
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        unsafe {
            FreeCredentialsHandle(&raw const self.handle);
        }
    }
}

/// Connection's peer certificate in DER encoding.
#[derive(Clone, Debug)]
pub struct PeerCert(pub Vec<u8>);

#[derive(Debug)]
/// An implementation of TLS streams backed by Windows Schannel.
pub struct SchannelFilter {
    inner: UnsafeCell<Schannel>,
}

#[derive(Debug)]
struct Schannel {
    ctx: Context,
    state: State,
    /// Handshake error, reported by `connect()` or `accept()` after the alert is flushed
    error: Option<io::Error>,
    /// Peer sent `close_notify`
    peer_closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Handshaking,
    Streaming,
    /// `close_notify` has been queued
    Closed,
    /// The handshake failed and an alert has been queued
    Failed,
    /// Post-handshake exchange, writes wait until it completes
    Renegotiating,
}

impl FilterLayer for SchannelFilter {
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        const H2: &[u8] = b"h2";

        let inner = self.inner();
        if matches!(inner.state, State::Handshaking | State::Failed) {
            return None;
        }

        if id == any::TypeId::of::<types::HttpProtocol>() {
            let proto = if inner.ctx.alpn_protocol().as_deref() == Some(H2) {
                types::HttpProtocol::Http2
            } else {
                types::HttpProtocol::Http1
            };
            Some(Box::new(proto))
        } else if id == any::TypeId::of::<PeerCert>() {
            inner
                .ctx
                .peer_cert()
                .map(|cert| Box::new(PeerCert(cert)) as Box<dyn any::Any>)
        } else if id == any::TypeId::of::<Servername>() {
            inner
                .ctx
                .servername
                .clone()
                .map(|name| Box::new(Servername(name)) as Box<dyn any::Any>)
        } else {
            None
        }
    }

    fn shutdown(&self, buf: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        let inner = self.inner_mut();
        if inner.state == State::Streaming {
            // pending application data has been encrypted by process_write_buf
            inner.state = State::Closed;
            buf.with_write_buffers(|_, dst| inner.ctx.close_notify(dst))?;
        }
        // wait for the peer's close_notify, or for a post-handshake exchange
        // to complete, writes are held back until then and close_notify is
        // sent once streaming resumes, unless the peer already closed
        // the connection and it is never going to arrive
        if matches!(inner.state, State::Closed | State::Renegotiating)
            && !inner.peer_closed
            && !buf.io().is_read_eof()
        {
            Ok(Poll::Pending)
        } else {
            Ok(Poll::Ready(()))
        }
    }

    fn process_read_buf(&self, rb: &FilterBuf<'_>) -> io::Result<()> {
        let inner = self.inner_mut();
        loop {
            match inner.state {
                State::Failed => return Ok(()),
                State::Handshaking | State::Renegotiating => {
                    if !inner.handshake(rb)? {
                        return Ok(());
                    }
                }
                State::Streaming | State::Closed => {}
            }

            let res = rb.with_read_buffers(|r_src, r_dst| -> io::Result<Decrypted> {
                if let Some(src) = r_src {
                    if inner.peer_closed {
                        src.clear();
                        return Ok(Decrypted::Pending);
                    }
                    while !src.is_empty() {
                        match inner.ctx.decrypt(src, r_dst)? {
                            Decrypted::Progress => {}
                            Decrypted::Pending => break,
                            res @ (Decrypted::Closed | Decrypted::Renegotiate) => return Ok(res),
                        }
                    }
                }
                Ok(Decrypted::Pending)
            })?;
            match res {
                Decrypted::Closed => {
                    inner.peer_closed = true;
                    // peer sent close_notify, start graceful shutdown
                    rb.io().close();
                    return Ok(());
                }
                // the context is shut down, but a post-handshake message still
                // has to be processed before the next record can be decrypted
                Decrypted::Renegotiate if inner.state == State::Closed => {
                    rb.with_write_buffers(|_, dst| {
                        rb.with_read_src(|src| inner.ctx.handshake_step(src.as_mut(), dst))
                    })?;
                }
                Decrypted::Renegotiate => inner.state = State::Renegotiating,
                Decrypted::Progress | Decrypted::Pending => return Ok(()),
            }
        }
    }

    fn process_write_buf(&self, wb: &FilterBuf<'_>) -> io::Result<()> {
        let inner = self.inner_mut();
        if inner.state != State::Streaming {
            return Ok(());
        }
        inner.encrypt_writes(wb)
    }
}

impl Schannel {
    /// Drives the handshake with buffered input, returns `true` once it is done.
    fn handshake(&mut self, rb: &FilterBuf<'_>) -> io::Result<bool> {
        let renegotiating = self.state == State::Renegotiating;
        loop {
            if renegotiating && !self.decrypt_before_handshake(rb)? {
                return Ok(false);
            }
            let input = rb.with_read_src(|src| src.as_ref().map_or(0, ntex_bytes::BytesMut::len));
            let state = rb.with_write_buffers(|_, dst| {
                let len = dst.len();
                rb.with_read_src(|src| self.ctx.handshake_step(src.as_mut(), dst))
                    .or_else(|err| {
                        if renegotiating || dst.len() == len {
                            Err(err)
                        } else {
                            // keep the io open until the handshake flushes the alert
                            self.state = State::Failed;
                            self.error = Some(err);
                            Ok(HandshakeState::NeedRead)
                        }
                    })
            })?;
            match state {
                HandshakeState::Done => break,
                // application data follows the consumed messages
                HandshakeState::NeedRead if renegotiating && Self::app_data_next(rb, input) => {}
                HandshakeState::NeedRead => return Ok(false),
                HandshakeState::Continue => {}
            }
        }
        self.state = State::Streaming;
        if renegotiating {
            // writes were held back during the exchange
            self.encrypt_writes(rb)?;
        }
        Ok(true)
    }

    /// Decrypts application data the peer sent before its messages of a
    /// post-handshake exchange.
    ///
    /// Schannel cannot process application data received during a TLS 1.2
    /// renegotiation, the connection fails instead.
    ///
    /// Returns `false` if an incomplete application data record is next.
    fn decrypt_before_handshake(&mut self, rb: &FilterBuf<'_>) -> io::Result<bool> {
        rb.with_read_buffers(|src, dst| {
            let Some(src) = src else {
                return Ok(true);
            };
            while src.first() == Some(&context::CONTENT_APPLICATION_DATA) {
                let res = self.ctx.decrypt(src, dst).map_err(|err| {
                    io::Error::new(
                        err.kind(),
                        format!("application data received during renegotiation: {err}"),
                    )
                })?;
                match res {
                    Decrypted::Progress => {}
                    Decrypted::Pending => return Ok(false),
                    // the record is a handshake message now
                    Decrypted::Renegotiate => break,
                    Decrypted::Closed => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "peer closed the connection during renegotiation",
                        ));
                    }
                }
            }
            Ok(true)
        })
    }

    /// Checks if input was consumed and an application data record is next.
    fn app_data_next(rb: &FilterBuf<'_>, input: usize) -> bool {
        rb.with_read_src(|src| {
            src.as_ref().is_some_and(|src| {
                src.len() < input && src.first() == Some(&context::CONTENT_APPLICATION_DATA)
            })
        })
    }

    fn encrypt_writes(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|src, dst| self.ctx.encrypt_pages(src, dst))
    }
}

impl SchannelFilter {
    fn inner(&self) -> &Schannel {
        // SAFETY: the filter is single-threaded and no method re-enters the
        // filter while it holds a reference to the state.
        unsafe { &*self.inner.get() }
    }

    #[allow(clippy::mut_from_ref)]
    fn inner_mut(&self) -> &mut Schannel {
        // SAFETY: see inner().
        unsafe { &mut *self.inner.get() }
    }

    fn start_handshake(&self, buf: &FilterBuf<'_>) -> io::Result<HandshakeState> {
        let inner = self.inner_mut();
        // a server waits for the ClientHello, buffered input has been passed
        // to the filter when it was added
        buf.with_write_buffers(|_, dst| {
            let state = inner.ctx.handshake_step(None, dst)?;
            if state == HandshakeState::Done {
                inner.state = State::Streaming;
            }
            Ok(state)
        })
    }

    fn is_handshaking(&self) -> bool {
        self.inner().state == State::Handshaking
    }

    fn take_error(&self) -> Option<io::Error> {
        self.inner_mut().error.take()
    }
}

/// Performs a client TLS handshake over `io`.
pub async fn connect<F: Filter>(
    io: Io<F>,
    domain: &str,
    config: ClientConfig,
) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    handshake(io, Context::client(domain, &config)?).await
}

/// Performs a server TLS handshake over `io`.
pub async fn accept<F: Filter>(
    io: Io<F>,
    config: &ServerConfig,
) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    handshake(io, Context::server(config)).await
}

async fn handshake<F: Filter>(io: Io<F>, ctx: Context) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    let filter = SchannelFilter {
        inner: UnsafeCell::new(Schannel {
            ctx,
            state: State::Handshaking,
            error: None,
            peer_closed: false,
        }),
    };
    let io = io.add_filter(filter);

    let state = io.with_buf(|buf| io.filter().start_handshake(buf))??;
    io.flush(false).await?;

    if state == HandshakeState::Done {
        return Ok(io);
    }

    // the read that reports eof may also carry the peer's last handshake flight
    let mut eof = false;
    loop {
        if let Some(err) = io.filter().take_error() {
            // make sure the alert reaches the peer before the io is dropped
            let _ = io.flush(true).await;
            return Err(err);
        }
        if !io.filter().is_handshaking() {
            return Ok(io);
        }
        if eof {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "disconnected"));
        }

        if io.read_notify().await?.is_none() {
            eof = true;
        }
        io.flush(false).await?;
    }
}

fn alpn_buffer<T: AsRef<[u8]>>(protocols: &[T]) -> Option<Arc<[u8]>> {
    // Layout for SECBUFFER_APPLICATION_PROTOCOLS:
    // u32 ProtocolListsSize, then one or more protocol lists.
    // Each protocol list is u32 negotiation extension, u16 list size, then ALPN wire list.
    if protocols.is_empty() {
        return None;
    }
    let mut wire = Vec::new();
    for proto in protocols {
        let proto = proto.as_ref();
        let len = u8::try_from(proto.len())
            .ok()
            .filter(|len| *len != 0)
            .expect("ALPN protocol must be 1..=255 bytes");
        wire.push(len);
        wire.extend_from_slice(proto);
    }
    let list_size = u16::try_from(wire.len()).expect("ALPN protocol list fits u16");
    let protocol_lists_size = 4u32 + 2 + u32::from(list_size);

    let mut buf = Vec::with_capacity(4 + protocol_lists_size as usize);
    buf.extend_from_slice(&protocol_lists_size.to_ne_bytes());
    buf.extend_from_slice(&(SecApplicationProtocolNegotiationExt_ALPN as u32).to_ne_bytes());
    buf.extend_from_slice(&list_size.to_ne_bytes());
    buf.extend_from_slice(&wire);
    Some(buf.into())
}

fn sspi_error(context: &'static str, status: windows_sys::core::HRESULT) -> io::Error {
    io::Error::other(SspiError { context, status })
}

#[derive(Debug)]
struct SspiError {
    context: &'static str,
    status: windows_sys::core::HRESULT,
}

impl std::fmt::Display for SspiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = u32::from_ne_bytes(self.status.to_ne_bytes());
        write!(f, "{} failed with HRESULT 0x{status:08X}", self.context)
    }
}

impl std::error::Error for SspiError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Credentials errors are reported, Schannel rejects a client key that
    /// is not persisted.
    #[cfg(feature = "openssl")]
    #[test]
    fn test_credentials_error_is_reported() {
        use tls_openssl::{asn1::Asn1Time, hash::MessageDigest, pkcs12::Pkcs12, pkey::PKey};
        use tls_openssl::{rsa::Rsa, x509::X509Builder, x509::X509NameBuilder};
        use windows_sys::Win32::Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CertCloseStore, CertDuplicateCertificateContext,
            CertEnumCertificatesInStore, PFXImportCertStore, PKCS12_ALWAYS_CNG_KSP,
            PKCS12_NO_PERSIST_KEY,
        };

        assert!(supports_sch_credentials());

        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "ntex client").unwrap();
        let name = name.build();
        let mut builder = X509Builder::new().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        builder.sign(&key, MessageDigest::sha256()).unwrap();
        let cert = builder.build();
        let pfx = Pkcs12::builder()
            .pkey(&key)
            .cert(&cert)
            .build2("")
            .unwrap()
            .to_der()
            .unwrap();

        let blob = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(pfx.len()).unwrap(),
            pbData: pfx.as_ptr().cast_mut(),
        };
        let password = [0u16];
        let cert = unsafe {
            let store = PFXImportCertStore(
                &raw const blob,
                password.as_ptr(),
                PKCS12_NO_PERSIST_KEY | PKCS12_ALWAYS_CNG_KSP,
            );
            assert!(!store.is_null());
            let cert =
                CertDuplicateCertificateContext(CertEnumCertificatesInStore(store, ptr::null()));
            CertCloseStore(store, 0);
            Certificate::from_context(cert)
        };

        let err = Credentials::acquire(false, Some(&cert)).unwrap_err();
        let err = err.get_ref().unwrap().downcast_ref::<SspiError>().unwrap();
        assert_eq!(
            err.status,
            windows_sys::Win32::Foundation::SEC_E_UNKNOWN_CREDENTIALS
        );
    }

    #[test]
    fn test_shared_credentials() {
        let cfg = ClientConfig::new();
        let cred = cfg.credentials().unwrap();
        assert!(Arc::ptr_eq(&cred, &cfg.credentials().unwrap()));
        assert!(Arc::ptr_eq(&cred, &cfg.clone().credentials().unwrap()));

        // validation mode is part of the credentials
        let danger = cfg.clone().danger_accept_invalid_certs(true);
        assert!(!Arc::ptr_eq(&cred, &danger.credentials().unwrap()));
        assert!(Arc::ptr_eq(&cred, &cfg.credentials().unwrap()));
    }

    #[test]
    fn test_alpn_buffer() {
        assert!(alpn_buffer::<&[u8]>(&[]).is_none());

        let buf = alpn_buffer(&[b"h2".as_slice(), b"http/1.1".as_slice()]).unwrap();
        let wire = b"\x02h2\x08http/1.1";
        assert_eq!(&buf[..4], &(4u32 + 2 + 12).to_ne_bytes());
        assert_eq!(
            &buf[4..8],
            &(SecApplicationProtocolNegotiationExt_ALPN as u32).to_ne_bytes()
        );
        assert_eq!(&buf[8..10], &12u16.to_ne_bytes());
        assert_eq!(&buf[10..], wire);
    }

    #[test]
    #[should_panic(expected = "ALPN protocol must be 1..=255 bytes")]
    fn test_alpn_empty_protocol() {
        let _ = ClientConfig::new().set_alpn_protocols(&[b"".as_slice()]);
    }
}
