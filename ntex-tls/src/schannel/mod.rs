//! TLS services and I/O filters backed by Windows Schannel.
//!
//! Available on Windows with the `schannel` feature. [`TlsAcceptor`] and
//! [`TlsConnector`] apply [`crate::TlsConfig`] to service handshakes; the free
//! [`accept()`] and [`connect()`] functions do not impose a timeout or concurrency
//! limit. All entry points preserve existing I/O filters below TLS.
//!
//! [`ClientConfig`] verifies server certificates and names by default using
//! Windows policy. [`ServerConfig::request_client_cert`] requests an optional
//! certificate but leaves its trust validation to the application. Certificate
//! identities come from a PFX archive or an existing Windows certificate store.
//!
//! Protocol versions and cipher suites follow system policy and OS support.
//! Both configurations offer `h2` and `http/1.1` by default. Metadata queries
//! expose [`PeerCert`], the backend-independent DER certificate types, received
//! SNI, TLS version, cipher, and [`ntex_io::types::HttpProtocol`]. Only negotiated
//! `h2` reports HTTP/2; absent or other ALPN values report HTTP/1.
#![cfg(windows)]
use std::sync::{Arc, Mutex, PoisonError};
use std::{any, cell::UnsafeCell, io, mem, ptr, task::Poll};

use ntex_bytes::Bytes;
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

mod accept;
mod cert;
mod connect;
mod context;

use self::context::{Context, Decrypted, HandshakeState};
use crate::{PeerCertChainDer, PeerCertDer, Servername, TlsCipher, TlsVersion};

pub use self::accept::TlsAcceptor;
pub use self::cert::{CertStoreLocation, Certificate};
pub use self::connect::TlsConnector;

/// Windows Schannel client configuration.
///
/// Clones share the credentials handle, so connections made with the same
/// configuration can resume TLS sessions.
/// Changing the certificate or verification mode creates a separate credential
/// cache for the modified configuration; existing clones keep their old cache.
///
/// Schannel offers a cached session's protocol version only. After a TLS 1.2
/// session with a server, the next connection to the same server name offers
/// TLS 1.2 only and fails if the server no longer supports it. The cache
/// expires after 10 hours by default (`ClientCacheTime`); a new configuration
/// starts with an empty cache.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Whether Schannel performs automatic certificate and server-name validation.
    verify: bool,
    /// Prebuilt `SECBUFFER_APPLICATION_PROTOCOLS` buffer, `None` disables ALPN.
    alpn: Option<Arc<[u8]>>,
    /// Optional client identity, retained for as long as its credentials are in use.
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
    /// Constructs a configuration with server verification enabled and no client identity.
    ///
    /// ALPN offers `h2` and `http/1.1`, in that order. Schannel does not
    /// automatically choose a client certificate from a Windows store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            verify: true,
            alpn: alpn_buffer(DEFAULT_ALPN),
            cert: None,
            cred: Arc::default(),
        }
    }

    /// Controls whether invalid server certificates and hostnames are accepted.
    ///
    /// Passing `true` disables automatic peer authentication and permits
    /// impersonation; use only when authentication is handled separately.
    /// Passing `false` restores validation. Either call detaches the credential
    /// cache, leaving other clones unchanged.
    #[must_use]
    pub fn danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> Self {
        self.verify = !accept_invalid_certs;
        // credentials depend on the validation mode
        self.cred = Arc::default();
        self
    }

    /// Set ALPN protocols offered to the server, in preference order.
    ///
    /// An empty list disables ALPN. This does not replace the shared credentials.
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
    /// Without it, no certificate is sent. The certificate and its private key
    /// are retained by the configuration and acquired credentials. This creates
    /// a fresh credential cache without changing other clones.
    #[must_use]
    pub fn set_client_cert(mut self, cert: Certificate) -> Self {
        self.cert = Some(cert);
        // credentials carry the certificate
        self.cred = Arc::default();
        self
    }

    /// Acquires credentials once per shared cache, retaining only successful acquisitions.
    ///
    /// The lock serializes initialization across clones and threads; session
    /// resumption state stays attached to the resulting Schannel handle.
    fn credentials(&self) -> io::Result<Arc<Credentials>> {
        let mut cred = self.cred.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cred) = &*cred {
            return Ok(cred.clone());
        }
        let new = Arc::new(Credentials::outbound(self.verify, self.cert.clone())?);
        *cred = Some(new.clone());
        Ok(new)
    }
}

/// Windows Schannel server configuration.
///
/// Clones share the credentials handle, so Schannel can resume TLS sessions
/// of connections accepted with the same configuration.
/// A client certificate is not requested by default, and protocol versions and
/// cipher suites follow Windows policy.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Prebuilt `SECBUFFER_APPLICATION_PROTOCOLS` buffer, `None` disables ALPN.
    alpn: Option<Arc<[u8]>>,
    /// Requests proof of client key possession, not certificate trust validation.
    request_client_cert: bool,
    /// Shared server credentials that retain the configured identity.
    cred: Arc<Credentials>,
}

impl ServerConfig {
    /// Construct Schannel server configuration with the server certificate.
    ///
    /// ALPN accepts `h2` and `http/1.1`, in that order. Client certificates are
    /// not requested. Credentials are acquired immediately and retain `cert`
    /// for the lifetime of configurations and connections sharing them.
    ///
    /// # Errors
    ///
    /// Fails if Schannel rejects the certificate, for example if its private
    /// key is not accessible.
    pub fn new(cert: Certificate) -> io::Result<Self> {
        Ok(Self {
            alpn: alpn_buffer(DEFAULT_ALPN),
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

/// Shared Schannel credentials, released when the last configuration or context drops.
struct Credentials {
    /// Initialized handle owned by this value and freed by `FreeCredentialsHandle`.
    handle: SecHandle,
    /// Keeps the certificate alive
    _cert: Option<Certificate>,
}

// SAFETY: Schannel credentials handles can be used from multiple threads,
// and `Certificate` only holds a reference-counted certificate context.
unsafe impl Send for Credentials {}
// SAFETY: see `Send`, the handle is only read after acquisition.
unsafe impl Sync for Credentials {}

impl Credentials {
    /// Acquires client credentials.
    fn outbound(verify: bool, cert: Option<Certificate>) -> io::Result<Self> {
        // never pick a client certificate from the user's store on its own
        let mut flags = SCH_USE_STRONG_CRYPTO | SCH_CRED_NO_DEFAULT_CREDS;
        if verify {
            flags |= SCH_CRED_AUTO_CRED_VALIDATION;
        } else {
            flags |= SCH_CRED_MANUAL_CRED_VALIDATION | SCH_CRED_NO_SERVERNAME_CHECK;
        }
        Self::acquire(SECPKG_CRED_OUTBOUND, flags, cert)
    }

    /// Acquires server credentials.
    fn inbound(cert: Certificate) -> io::Result<Self> {
        // do not map client certificates to Windows accounts
        let flags = SCH_USE_STRONG_CRYPTO | SCH_CRED_NO_SYSTEM_MAPPER;
        Self::acquire(SECPKG_CRED_INBOUND, flags, Some(cert))
    }

    /// Acquires inbound or outbound credentials using the OS-supported configuration structure.
    ///
    /// Retains the certificate alongside the handle. Failure is reported
    /// directly; a modern-credentials error does not trigger a legacy retry.
    fn acquire(direction: u32, flags: u32, cert: Option<Certificate>) -> io::Result<Self> {
        // Schannel keeps its own reference to the certificate
        let mut certs = cert.as_ref().map(|cert| cert.as_ptr().cast_mut());
        let (num_certs, certs_ptr) = match &mut certs {
            Some(cert) => (1, ptr::from_mut(cert)),
            None => (0, ptr::null_mut()),
        };

        let mut sch_cred;
        let mut schannel_cred;
        let auth_data = if supports_sch_credentials() {
            // SCH_CREDENTIALS enables the system default protocols, including TLS 1.3
            sch_cred = SCH_CREDENTIALS {
                dwVersion: SCH_CREDENTIALS_VERSION,
                dwFlags: flags,
                cCreds: num_certs,
                paCred: certs_ptr,
                ..Default::default()
            };
            (&raw mut sch_cred).cast()
        } else {
            schannel_cred = SCHANNEL_CRED {
                dwVersion: SCHANNEL_CRED_VERSION,
                dwFlags: flags,
                cCreds: num_certs,
                paCred: certs_ptr,
                ..Default::default()
            };
            (&raw mut schannel_cred).cast()
        };

        let mut handle = SecHandle::default();
        let mut expiry = 0i64;
        // SAFETY: `auth_data` points to an initialized credentials structure whose
        // certificate array outlives the call, the out pointers are locals.
        let status = unsafe {
            AcquireCredentialsHandleW(
                ptr::null(),
                UNISP_NAME_W,
                direction,
                ptr::null(),
                auth_data,
                None,
                ptr::null(),
                &raw mut handle,
                &raw mut expiry,
            )
        };
        if status != SEC_E_OK {
            return Err(sspi_error("AcquireCredentialsHandleW", status));
        }
        Ok(Self {
            handle,
            _cert: cert,
        })
    }
}

/// `SCH_CREDENTIALS` is supported since Windows 10 1809, earlier versions
/// support `SCHANNEL_CRED` only.
///
/// Errors of `SCH_CREDENTIALS` are reported as is, a fallback to
/// `SCHANNEL_CRED` would hide them and offer older protocols only.
fn supports_sch_credentials() -> bool {
    let mut info = OSVERSIONINFOW::default();
    #[allow(clippy::cast_possible_truncation)]
    {
        info.dwOSVersionInfoSize = u32::try_from(mem::size_of::<OSVERSIONINFOW>()).unwrap_or(0);
    }
    // unlike GetVersionExW, not affected by the application manifest
    // SAFETY: `info` is an `OSVERSIONINFOW` with its size field set.
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
        // SAFETY: the handle was acquired by `acquire` and is freed once.
        unsafe {
            FreeCredentialsHandle(&raw const self.handle);
        }
    }
}

/// The peer's end-entity certificate in DER encoding.
///
/// Equivalent to querying [`PeerCertDer`]. No value is returned if the peer
/// supplied no certificate. In particular, a requested client certificate
/// still requires application-level trust validation.
#[derive(Clone, Debug)]
pub struct PeerCert(
    /// Owned DER-encoded certificate bytes.
    pub Vec<u8>,
);

#[derive(Debug)]
/// Encrypts and decrypts either side of a Windows Schannel TLS connection.
///
/// Created by this module's handshake functions and services. Application
/// writes wait during post-handshake negotiation. Shutdown sends `close_notify`
/// after pending writes and waits for peer closure or transport EOF.
pub struct SchannelFilter {
    /// Single-threaded state, borrowed only within non-reentrant filter operations.
    inner: UnsafeCell<Schannel>,
}

/// Per-connection record-processing and handshake state.
#[derive(Debug)]
struct Schannel {
    /// Owns the SSPI context and the credentials keeping its identity alive.
    ctx: Context,
    /// Controls when records may be decrypted, application writes encrypted, or closure sent.
    state: State,
    /// Handshake error, reported by `connect()` or `accept()` after the alert is flushed
    error: Option<io::Error>,
    /// Peer sent `close_notify`
    peer_closed: bool,
}

/// Lifecycle governing handshake driving, application writes, and close-alert generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Initial negotiation; application writes remain queued.
    Handshaking,
    /// Established connection able to encrypt application writes.
    Streaming,
    /// `close_notify` has been queued
    Closed,
    /// The handshake failed and an alert has been queued
    Failed,
    /// Post-handshake exchange, writes wait until it completes
    Renegotiating,
}

impl FilterLayer for SchannelFilter {
    /// Exposes established-session metadata; initial and failed handshakes expose none.
    fn query(&self, id: any::TypeId) -> Option<Box<dyn any::Any>> {
        let inner = self.inner();
        if matches!(inner.state, State::Handshaking | State::Failed) {
            return None;
        }

        if id == any::TypeId::of::<types::HttpProtocol>() {
            let alpn = inner.ctx.alpn_protocol();
            Some(Box::new(crate::utils::http_protocol(alpn)))
        } else if id == any::TypeId::of::<PeerCert>() {
            Some(Box::new(PeerCert(inner.ctx.peer_cert()?)))
        } else if id == any::TypeId::of::<PeerCertDer>() {
            Some(Box::new(PeerCertDer(Bytes::from(inner.ctx.peer_cert()?))))
        } else if id == any::TypeId::of::<PeerCertChainDer>() {
            Some(Box::new(PeerCertChainDer(inner.ctx.peer_cert_chain()?)))
        } else if id == any::TypeId::of::<Servername>() {
            Some(Box::new(Servername(inner.ctx.servername.clone()?)))
        } else if id == any::TypeId::of::<TlsVersion>() {
            Some(Box::new(inner.ctx.protocol_version()?))
        } else if id == any::TypeId::of::<TlsCipher>() {
            Some(Box::new(inner.ctx.cipher()?))
        } else {
            None
        }
    }

    /// Defers closure during renegotiation, otherwise sends the alert once and waits for the peer.
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

    /// Processes records and releases the state borrow before notifying I/O of peer closure.
    fn process_read_buf(&self, rb: &FilterBuf<'_>) -> io::Result<()> {
        let (result, peer_closed) = {
            #[cfg(test)]
            let _borrow = crate::utils::testing::TlsBorrow::new();

            let inner = self.inner_mut();
            if inner.peer_closed {
                // input after the peer's close_notify is ignored
                rb.with_read_src(|src| src.as_mut().map(ntex_bytes::BytesMut::clear));
                return Ok(());
            }
            let result = inner.process_read_buf(rb);
            (result, inner.peer_closed)
        };
        // Release the state before closing: the read-task waker may reenter.
        if peer_closed {
            rb.io().close();
        }
        result
    }

    /// Encrypts application data only in the streaming state, preserving it otherwise.
    fn process_write_buf(&self, wb: &FilterBuf<'_>) -> io::Result<()> {
        let inner = self.inner_mut();
        if inner.state != State::Streaming {
            return Ok(());
        }
        inner.encrypt_writes(wb)
    }
}

impl Schannel {
    /// Alternates handshake and record processing until more input is needed.
    ///
    /// Post-handshake messages may still need SSPI processing after a local
    /// close alert, but must not reopen the connection for application writes.
    fn process_read_buf(&mut self, rb: &FilterBuf<'_>) -> io::Result<()> {
        loop {
            match self.state {
                State::Failed => return Ok(()),
                State::Handshaking | State::Renegotiating => {
                    if !self.handshake(rb)? {
                        return Ok(());
                    }
                }
                State::Streaming | State::Closed => {}
            }

            match self.decrypt(rb)? {
                // the context is shut down, but a post-handshake message still
                // has to be processed before the next record can be decrypted
                Decrypted::Renegotiate if self.state == State::Closed => {
                    rb.with_write_buffers(|_, dst| {
                        rb.with_read_src(|src| self.ctx.handshake_step(src.as_mut(), dst))
                    })?;
                }
                Decrypted::Renegotiate => self.state = State::Renegotiating,
                Decrypted::Closed | Decrypted::Progress | Decrypted::Pending => return Ok(()),
            }
        }
    }

    /// Drives the handshake with buffered input, returns `true` once it is done.
    ///
    /// Initial failures that generated an alert are saved until the outer
    /// handshake driver can flush it. Post-handshake failures propagate
    /// immediately. Completing renegotiation also resumes queued writes.
    fn handshake(&mut self, rb: &FilterBuf<'_>) -> io::Result<bool> {
        let renegotiating = self.state == State::Renegotiating;
        loop {
            let input = Self::input_len(rb);
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
                HandshakeState::NeedRead if renegotiating => {
                    // application data and alerts may precede the peer's next message
                    if self.decrypt(rb)? == Decrypted::Closed {
                        return Ok(false);
                    }
                    let rest = Self::input_len(rb);
                    if rest == 0 || rest == input {
                        return Ok(false);
                    }
                }
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

    /// Decrypts buffered records, during a post-handshake exchange up to the
    /// peer's next handshake message.
    ///
    /// During a TLS 1.2 renegotiation Schannel decrypts records only after
    /// the `ClientHello` has been generated.
    fn decrypt(&mut self, rb: &FilterBuf<'_>) -> io::Result<Decrypted> {
        let renegotiating = self.state == State::Renegotiating;
        let res = rb.with_read_buffers(|src, dst| -> io::Result<Decrypted> {
            let Some(src) = src else {
                return Ok(Decrypted::Pending);
            };
            // the peer's next handshake message goes to ISC or ASC
            while !src.is_empty() && (!renegotiating || context::handshake_records_len(src) == 0) {
                match self.ctx.decrypt(src, dst)? {
                    Decrypted::Progress => {}
                    res => return Ok(res),
                }
            }
            Ok(Decrypted::Pending)
        })?;
        if res == Decrypted::Closed {
            self.peer_closed = true;
        }
        Ok(res)
    }

    /// Measures buffered ciphertext to detect lack of progress during renegotiation.
    fn input_len(rb: &FilterBuf<'_>) -> usize {
        rb.with_read_src(|src| src.as_ref().map_or(0, ntex_bytes::BytesMut::len))
    }

    /// Encrypts pending plaintext into the lower filter's output pages.
    fn encrypt_writes(&mut self, buf: &FilterBuf<'_>) -> io::Result<()> {
        buf.with_write_buffers(|src, dst| self.ctx.encrypt_pages(src, dst))
    }
}

impl SchannelFilter {
    /// Borrows state without overlapping any mutable or reentrant filter operation.
    fn inner(&self) -> &Schannel {
        // SAFETY: the filter is single-threaded and no method re-enters the
        // filter while it holds a reference to the state.
        unsafe { &*self.inner.get() }
    }

    /// Borrows state exclusively; callers must release it before reentrant I/O notifications.
    #[allow(clippy::mut_from_ref)]
    fn inner_mut(&self) -> &mut Schannel {
        // SAFETY: see inner().
        unsafe { &mut *self.inner.get() }
    }

    /// Reports only initial negotiation; post-handshake exchanges are driven by the filter.
    fn is_handshaking(&self) -> bool {
        self.inner().state == State::Handshaking
    }

    /// Transfers a deferred initial-handshake error to the outer driver for alert flushing.
    fn take_error(&self) -> Option<io::Error> {
        self.inner_mut().error.take()
    }
}

/// Performs a client TLS handshake over `io`.
///
/// `domain` is passed to Schannel as the target name for authentication and SNI;
/// supply a DNS name or IP address without a port or IPv6 brackets. Existing
/// filters are preserved. No handshake timeout or concurrency limit is applied;
/// [`TlsConnector`] adds the service-configured timeout.
///
/// # Errors
///
/// Returns credential-acquisition, verification, negotiation, or transport
/// errors. Premature transport EOF returns [`io::ErrorKind::UnexpectedEof`].
pub async fn connect<F: Filter>(
    io: Io<F>,
    domain: &str,
    config: ClientConfig,
) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    handshake(io, Context::client(domain, &config)?).await
}

/// Performs a server TLS handshake over `io`.
///
/// Preserves existing filters and applies neither a handshake timeout nor the
/// acceptor concurrency limit. Use [`TlsAcceptor`] for those service policies.
///
/// Plaintext handshake reassembly is limited to 64 KiB per message, including
/// its four-byte header, and 1024 records. Exceeding either limit or receiving an
/// empty plaintext handshake record fails with [`io::ErrorKind::InvalidData`].
///
/// # Errors
///
/// Returns negotiation or transport errors in addition to the reassembly errors
/// above. Premature transport EOF returns [`io::ErrorKind::UnexpectedEof`].
pub async fn accept<F: Filter>(
    io: Io<F>,
    config: &ServerConfig,
) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    handshake(io, Context::server(config)).await
}

/// Installs the role-specific context and drives initial negotiation, flushing deferred alerts.
async fn handshake<F: Filter>(io: Io<F>, ctx: Context) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    let filter = SchannelFilter {
        inner: UnsafeCell::new(Schannel {
            ctx,
            state: State::Handshaking,
            error: None,
            peer_closed: false,
        }),
    };
    // the filter starts the handshake with the buffered input when it is
    // added, a client queues its ClientHello
    let io = io.add_filter(filter);
    crate::utils::handshake(&io, || match io.filter().take_error() {
        Some(err) => Err(err),
        None => Ok(io.filter().is_handshaking()),
    })
    .await?;
    Ok(io)
}

/// ALPN protocols offered or accepted by default.
const DEFAULT_ALPN: &[&[u8]] = &[b"h2", b"http/1.1"];

/// Encodes ALPN wire names inside a native-endian SSPI application-protocols buffer.
///
/// An empty list disables ALPN. Panics for empty or oversized protocol names,
/// or when the length-prefixed wire list cannot fit in a `u16`.
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

/// Wraps an SSPI failure without losing its operation name or HRESULT bits.
fn sspi_error(context: &'static str, status: windows_sys::core::HRESULT) -> io::Error {
    io::Error::other(SspiError { context, status })
}

/// Diagnostic payload for an `io::ErrorKind::Other` SSPI failure.
#[derive(Debug)]
struct SspiError {
    /// Operation or queried attribute that failed.
    context: &'static str,
    /// Original signed HRESULT, formatted as an unsigned hexadecimal value.
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

    /// STATEMENT: Schannel reports a credentials error when a client key is not persisted.
    ///
    /// HOW:
    /// - Generate and import a client certificate using `PKCS12_NO_PERSIST_KEY`.
    /// - Request outbound credentials and check the underlying `SspiError` is `SEC_E_UNKNOWN_CREDENTIALS`.
    #[cfg(feature = "openssl")]
    #[test]
    fn test_credentials_error_is_reported() {
        use tls_openssl::{asn1::Asn1Time, hash::MessageDigest, pkcs12::Pkcs12, pkey::PKey};
        use tls_openssl::{rsa::Rsa, x509::X509Builder, x509::X509NameBuilder};
        use windows_sys::Win32::Security::Cryptography::{
            PKCS12_ALWAYS_CNG_KSP, PKCS12_NO_PERSIST_KEY,
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

        let store = crate::win::CertStore::from_pfx(
            &pfx,
            Some(""),
            PKCS12_NO_PERSIST_KEY | PKCS12_ALWAYS_CNG_KSP,
        )
        .unwrap();
        let (cert, ()) = store.find_map(|_| Some(())).unwrap();
        drop(store);
        let cert = Certificate::from_context(cert);

        let err = Credentials::outbound(false, Some(cert)).unwrap_err();
        let err = err.get_ref().unwrap().downcast_ref::<SspiError>().unwrap();
        assert_eq!(
            err.status,
            windows_sys::Win32::Foundation::SEC_E_UNKNOWN_CREDENTIALS
        );
    }

    /// STATEMENT: Client configurations share cached credentials until certificate-validation settings change.
    ///
    /// HOW:
    /// - Compare credential pointers across repeated calls and a configuration clone.
    /// - Disable validation in a clone and check only that clone gets a different credential allocation.
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

    /// STATEMENT: ALPN encoding produces the Schannel header and length-prefixed protocol list.
    ///
    /// HOW:
    /// - Check an empty protocol list produces no buffer.
    /// - Encode `h2` and `http/1.1` and compare header fields, native-endian lengths, and protocol bytes.
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

    /// STATEMENT: An empty ALPN protocol is rejected by configuration.
    ///
    /// HOW:
    /// - Set an ALPN list containing an empty protocol and require the length-validation panic.
    #[test]
    #[should_panic(expected = "ALPN protocol must be 1..=255 bytes")]
    fn test_alpn_empty_protocol() {
        let _ = ClientConfig::new().set_alpn_protocols(&[b"".as_slice()]);
    }
}
