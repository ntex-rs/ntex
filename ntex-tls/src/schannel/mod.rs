//! An implementation of TLS streams backed by Windows Schannel.
#![cfg(windows)]
use std::{any, cell::UnsafeCell, cmp, io, mem, ptr, slice, sync::Arc, task::Poll};

use ntex_bytes::{BufMut, BytesMut};
use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer, types};
use windows_sys::Win32::Foundation::{
    CRYPT_E_REVOKED, SEC_E_CERT_EXPIRED, SEC_E_CERT_UNKNOWN, SEC_E_INCOMPLETE_MESSAGE, SEC_E_OK,
    SEC_E_UNTRUSTED_ROOT, SEC_E_WRONG_PRINCIPAL, SEC_I_CONTEXT_EXPIRED, SEC_I_CONTINUE_NEEDED,
    SEC_I_RENEGOTIATE,
};
use windows_sys::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, ApplyControlToken, DecryptMessage, DeleteSecurityContext,
    EncryptMessage, FreeContextBuffer, FreeCredentialsHandle, ISC_REQ_ALLOCATE_MEMORY,
    ISC_REQ_CONFIDENTIALITY, ISC_REQ_EXTENDED_ERROR, ISC_REQ_REPLAY_DETECT,
    ISC_REQ_SEQUENCE_DETECT, ISC_REQ_STREAM, InitializeSecurityContextW, QueryContextAttributesW,
    SCH_CRED_AUTO_CRED_VALIDATION, SCH_CRED_MANUAL_CRED_VALIDATION, SCH_CRED_NO_SERVERNAME_CHECK,
    SCH_USE_STRONG_CRYPTO, SCHANNEL_ALERT, SCHANNEL_ALERT_TOKEN, SCHANNEL_CRED,
    SCHANNEL_CRED_VERSION, SCHANNEL_SHUTDOWN, SECBUFFER_APPLICATION_PROTOCOLS, SECBUFFER_DATA,
    SECBUFFER_EMPTY, SECBUFFER_EXTRA, SECBUFFER_STREAM_HEADER, SECBUFFER_STREAM_TRAILER,
    SECBUFFER_TOKEN, SECBUFFER_VERSION, SECPKG_ATTR_APPLICATION_PROTOCOL,
    SECPKG_ATTR_REMOTE_CERT_CONTEXT, SECPKG_ATTR_STREAM_SIZES, SECPKG_CRED_OUTBOUND,
    SECURITY_NATIVE_DREP, SecApplicationProtocolNegotiationExt_ALPN,
    SecApplicationProtocolNegotiationStatus_Success, SecBuffer, SecBufferDesc,
    SecPkgContext_ApplicationProtocol, SecPkgContext_StreamSizes, TLS1_ALERT_BAD_CERTIFICATE,
    TLS1_ALERT_CERTIFICATE_EXPIRED, TLS1_ALERT_CERTIFICATE_REVOKED, TLS1_ALERT_FATAL,
    TLS1_ALERT_HANDSHAKE_FAILURE, TLS1_ALERT_UNKNOWN_CA, UNISP_NAME_W,
};
use windows_sys::Win32::Security::Credentials::SecHandle;
use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CertDuplicateCertificateContext, CertFreeCertificateContext,
};

mod connect;
pub use self::connect::TlsConnector;

/// Windows Schannel client configuration.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    verify: bool,
    /// Prebuilt `SECBUFFER_APPLICATION_PROTOCOLS` buffer, `None` disables ALPN.
    alpn: Option<Arc<[u8]>>,
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
        }
    }

    /// Accept invalid server certificates and hostnames.
    #[must_use]
    pub fn danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> Self {
        self.verify = !accept_invalid_certs;
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
    /// Handshake error, reported by `connect()` after the alert is flushed
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decrypted {
    Progress,
    /// Needs more input
    Pending,
    /// Peer sent `close_notify`
    Closed,
}

struct Context {
    cred: SecHandle,
    ctxt: SecHandle,
    have_ctxt: bool,
    target: Vec<u16>,
    alpn: Option<Arc<[u8]>>,
    sizes: Option<SecPkgContext_StreamSizes>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("have_ctxt", &self.have_ctxt)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandshakeState {
    Done,
    NeedRead,
    /// More handshake input is already buffered, call again without reading.
    Continue,
}

impl Context {
    fn new(domain: &str, config: &ClientConfig) -> io::Result<Self> {
        let mut cred = unsafe { mem::zeroed::<SecHandle>() };
        let mut expiry = 0i64;
        let mut schannel_cred = unsafe { mem::zeroed::<SCHANNEL_CRED>() };
        schannel_cred.dwVersion = SCHANNEL_CRED_VERSION;
        schannel_cred.dwFlags = SCH_USE_STRONG_CRYPTO;
        if config.verify {
            schannel_cred.dwFlags |= SCH_CRED_AUTO_CRED_VALIDATION;
        } else {
            schannel_cred.dwFlags |= SCH_CRED_MANUAL_CRED_VALIDATION | SCH_CRED_NO_SERVERNAME_CHECK;
        }

        let status = unsafe {
            AcquireCredentialsHandleW(
                ptr::null(),
                UNISP_NAME_W,
                SECPKG_CRED_OUTBOUND,
                ptr::null(),
                (&raw mut schannel_cred).cast(),
                None,
                ptr::null(),
                &raw mut cred,
                &raw mut expiry,
            )
        };
        if status != SEC_E_OK {
            return Err(sspi_error("AcquireCredentialsHandleW", status));
        }

        Ok(Self {
            cred,
            ctxt: unsafe { mem::zeroed::<SecHandle>() },
            have_ctxt: false,
            target: domain.encode_utf16().chain(Some(0)).collect(),
            alpn: config.alpn.clone(),
            sizes: None,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn handshake_step(
        &mut self,
        input: Option<&mut BytesMut>,
        output: &mut ntex_bytes::BytePages,
    ) -> io::Result<HandshakeState> {
        let mut out_buf = SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: ptr::null_mut(),
        };
        let mut out_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 1,
            pBuffers: &raw mut out_buf,
        };

        let mut in_bufs = [
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: ptr::null_mut(),
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: ptr::null_mut(),
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: ptr::null_mut(),
            },
        ];
        // ALPN is part of the ClientHello, only the first call needs it
        if !self.have_ctxt
            && let Some(alpn) = self.alpn.as_ref()
        {
            in_bufs[2] = SecBuffer {
                cbBuffer: u32::try_from(alpn.len())
                    .map_err(|_| io::Error::other("TLS ALPN buffer is too large"))?,
                BufferType: SECBUFFER_APPLICATION_PROTOCOLS,
                pvBuffer: alpn.as_ptr().cast_mut().cast(),
            };
        }
        let in_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: u32::try_from(in_bufs.len()).expect("static SecBuffer count fits u32"),
            pBuffers: in_bufs.as_mut_ptr(),
        };

        let mut input_len = 0usize;
        if let Some(src) = input.as_ref() {
            input_len = src.len();
            if input_len != 0 {
                in_bufs[0].BufferType = SECBUFFER_TOKEN;
                in_bufs[0].cbBuffer = u32::try_from(input_len)
                    .map_err(|_| io::Error::other("TLS input buffer is too large"))?;
                in_bufs[0].pvBuffer = src.as_ptr().cast_mut().cast();
            }
        }
        let input_desc = &raw const in_desc;

        let mut attrs = 0u32;
        let mut expiry = 0i64;
        let ctxt = if self.have_ctxt {
            &raw const self.ctxt
        } else {
            ptr::null()
        };
        let status = unsafe {
            InitializeSecurityContextW(
                &raw const self.cred,
                ctxt,
                self.target.as_ptr(),
                ISC_FLAGS,
                0,
                SECURITY_NATIVE_DREP,
                input_desc,
                0,
                &raw mut self.ctxt,
                &raw mut out_desc,
                &raw mut attrs,
                &raw mut expiry,
            )
        };
        self.have_ctxt = true;
        let has_token = !out_buf.pvBuffer.is_null() && out_buf.cbBuffer != 0;
        take_token(&out_buf, output);

        if status == SEC_E_INCOMPLETE_MESSAGE {
            return Ok(HandshakeState::NeedRead);
        }
        if status != SEC_E_OK && status != SEC_I_CONTINUE_NEEDED {
            if !has_token {
                // e.g. certificate validation fails without an alert, best effort
                let _ = self.fatal_alert(status, output);
            }
            return Err(sspi_error("InitializeSecurityContextW", status));
        }

        let mut consumed = 0;
        let mut extra = 0;
        if let Some(src) = input {
            extra = in_bufs
                .iter()
                .find(|buf| buf.BufferType == SECBUFFER_EXTRA)
                .map_or(0, |buf| buf.cbBuffer as usize);
            consumed = input_len.saturating_sub(extra);
            if consumed != 0 {
                src.advance_to(consumed);
            }
        }

        if status == SEC_E_OK {
            self.query_stream_sizes()?;
            Ok(HandshakeState::Done)
        } else if extra != 0 && consumed != 0 {
            Ok(HandshakeState::Continue)
        } else {
            Ok(HandshakeState::NeedRead)
        }
    }

    /// Generates a `close_notify` alert and appends it to `output`.
    fn close_notify(&mut self, output: &mut ntex_bytes::BytePages) -> io::Result<()> {
        let mut token = SCHANNEL_SHUTDOWN;
        self.control_token(&mut token, "SCHANNEL_SHUTDOWN", output)
    }

    /// Generates a fatal alert for a failed handshake and appends it to `output`.
    fn fatal_alert(
        &mut self,
        status: windows_sys::core::HRESULT,
        output: &mut ntex_bytes::BytePages,
    ) -> io::Result<()> {
        let mut token = SCHANNEL_ALERT_TOKEN {
            dwTokenType: SCHANNEL_ALERT,
            dwAlertType: TLS1_ALERT_FATAL,
            dwAlertNumber: match status {
                SEC_E_UNTRUSTED_ROOT => TLS1_ALERT_UNKNOWN_CA,
                SEC_E_CERT_EXPIRED => TLS1_ALERT_CERTIFICATE_EXPIRED,
                CRYPT_E_REVOKED => TLS1_ALERT_CERTIFICATE_REVOKED,
                SEC_E_WRONG_PRINCIPAL | SEC_E_CERT_UNKNOWN => TLS1_ALERT_BAD_CERTIFICATE,
                _ => TLS1_ALERT_HANDSHAKE_FAILURE,
            },
        };
        self.control_token(&mut token, "SCHANNEL_ALERT", output)
    }

    /// Applies a control token and appends the resulting record to `output`.
    fn control_token<T>(
        &mut self,
        token: &mut T,
        name: &'static str,
        output: &mut ntex_bytes::BytePages,
    ) -> io::Result<()> {
        let mut in_buf = SecBuffer {
            cbBuffer: u32::try_from(mem::size_of::<T>()).expect("control token size fits u32"),
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: ptr::from_mut(token).cast(),
        };
        let in_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 1,
            pBuffers: &raw mut in_buf,
        };
        let status = unsafe { ApplyControlToken(&raw const self.ctxt, &raw const in_desc) };
        if status != SEC_E_OK {
            return Err(sspi_error(name, status));
        }

        let mut out_buf = SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: ptr::null_mut(),
        };
        let mut out_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 1,
            pBuffers: &raw mut out_buf,
        };
        let mut attrs = 0u32;
        let mut expiry = 0i64;
        let status = unsafe {
            InitializeSecurityContextW(
                &raw const self.cred,
                &raw const self.ctxt,
                self.target.as_ptr(),
                ISC_FLAGS,
                0,
                SECURITY_NATIVE_DREP,
                ptr::null(),
                0,
                &raw mut self.ctxt,
                &raw mut out_desc,
                &raw mut attrs,
                &raw mut expiry,
            )
        };
        take_token(&out_buf, output);

        if status == SEC_E_OK || status == SEC_I_CONTEXT_EXPIRED {
            Ok(())
        } else {
            Err(sspi_error(name, status))
        }
    }

    fn query_stream_sizes(&mut self) -> io::Result<SecPkgContext_StreamSizes> {
        if let Some(sizes) = self.sizes {
            return Ok(sizes);
        }

        let mut sizes = unsafe { mem::zeroed::<SecPkgContext_StreamSizes>() };
        let status = unsafe {
            QueryContextAttributesW(
                &raw const self.ctxt,
                SECPKG_ATTR_STREAM_SIZES,
                (&raw mut sizes).cast(),
            )
        };
        if status != SEC_E_OK {
            return Err(sspi_error(
                "QueryContextAttributesW(SECPKG_ATTR_STREAM_SIZES)",
                status,
            ));
        }
        self.sizes = Some(sizes);
        Ok(sizes)
    }

    /// Encrypts one TLS record from `src` into `dst`.
    ///
    /// The record is encrypted in place in the free space of the current
    /// destination page, shrinking it to fit. When the page has almost no room
    /// left, the record goes to a separate frame of at most one page.
    fn encrypt(&mut self, src: &[u8], dst: &mut ntex_bytes::BytePages) -> io::Result<usize> {
        // smallest record worth encrypting into the rest of the current page
        const MIN_RECORD: usize = 1024;

        let sizes = self.query_stream_sizes()?;
        let len = cmp::min(src.len(), sizes.cbMaximumMessage as usize);
        if len == 0 {
            return Ok(0);
        }
        let overhead = (sizes.cbHeader + sizes.cbTrailer) as usize;

        // a full page is pushed out by `with_bytes_mut`, the next record starts a new one
        let written = dst.with_bytes_mut(|page| {
            let avail = page.remaining_mut();
            if avail >= overhead + cmp::min(len, MIN_RECORD) {
                let len = cmp::min(len, avail - overhead);
                let tls_len =
                    self.encrypt_into(&src[..len], page.chunk_mut().as_mut_ptr(), sizes)?;
                unsafe { page.advance_mut(tls_len) };
                Ok(Some(len))
            } else {
                Ok::<_, io::Error>(None)
            }
        })?;
        if let Some(written) = written {
            return Ok(written);
        }

        let len = cmp::min(
            len,
            dst.page_size().capacity().saturating_sub(overhead).max(1),
        );
        let mut frame = BytesMut::with_capacity(overhead + len);
        let tls_len = self.encrypt_into(&src[..len], frame.chunk_mut().as_mut_ptr(), sizes)?;
        unsafe { frame.advance_mut(tls_len) };
        dst.append(frame);
        Ok(len)
    }

    /// Encrypts `src` into a record at `frame`, returns the record length.
    ///
    /// `frame` must be valid for writes of header, `src.len()` and trailer bytes.
    fn encrypt_into(
        &mut self,
        src: &[u8],
        frame: *mut u8,
        sizes: SecPkgContext_StreamSizes,
    ) -> io::Result<usize> {
        let header_len = sizes.cbHeader as usize;
        let len = src.len();
        unsafe { ptr::copy_nonoverlapping(src.as_ptr(), frame.add(header_len), len) };

        let mut bufs = [
            SecBuffer {
                cbBuffer: sizes.cbHeader,
                BufferType: SECBUFFER_STREAM_HEADER,
                pvBuffer: frame.cast(),
            },
            SecBuffer {
                cbBuffer: u32::try_from(len).expect("TLS message length fits u32"),
                BufferType: SECBUFFER_DATA,
                pvBuffer: unsafe { frame.add(header_len).cast() },
            },
            SecBuffer {
                cbBuffer: sizes.cbTrailer,
                BufferType: SECBUFFER_STREAM_TRAILER,
                pvBuffer: unsafe { frame.add(header_len + len).cast() },
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: ptr::null_mut(),
            },
        ];
        let desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: u32::try_from(bufs.len()).expect("static SecBuffer count fits u32"),
            pBuffers: bufs.as_mut_ptr(),
        };
        let status = unsafe { EncryptMessage(&raw const self.ctxt, 0, &raw const desc, 0) };
        if status != SEC_E_OK {
            return Err(sspi_error("EncryptMessage", status));
        }

        let tls_len = usize::try_from(bufs[0].cbBuffer).expect("TLS header length fits usize")
            + usize::try_from(bufs[1].cbBuffer).expect("TLS data length fits usize")
            + usize::try_from(bufs[2].cbBuffer).expect("TLS trailer length fits usize");
        Ok(tls_len)
    }

    fn decrypt(&mut self, src: &mut BytesMut, dst: &mut BytesMut) -> io::Result<Decrypted> {
        if src.is_empty() {
            return Ok(Decrypted::Pending);
        }

        let input_len = src.len();
        let mut bufs = [
            SecBuffer {
                cbBuffer: u32::try_from(input_len)
                    .map_err(|_| io::Error::other("TLS input buffer is too large"))?,
                BufferType: SECBUFFER_DATA,
                pvBuffer: src.as_mut_ptr().cast(),
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: ptr::null_mut(),
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: ptr::null_mut(),
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: ptr::null_mut(),
            },
        ];
        let desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: u32::try_from(bufs.len()).expect("static SecBuffer count fits u32"),
            pBuffers: bufs.as_mut_ptr(),
        };
        let mut qop = 0u32;
        let status =
            unsafe { DecryptMessage(&raw const self.ctxt, &raw const desc, 0, &raw mut qop) };

        match status {
            SEC_E_OK => {}
            SEC_E_INCOMPLETE_MESSAGE => return Ok(Decrypted::Pending),
            SEC_I_CONTEXT_EXPIRED => {
                // peer sent close_notify, data after it is ignored
                src.clear();
                return Ok(Decrypted::Closed);
            }
            SEC_I_RENEGOTIATE => {
                return Err(io::Error::other("TLS renegotiation is not supported"));
            }
            _ => return Err(sspi_error("DecryptMessage", status)),
        }

        let (data, extra) = decrypted_parts(&bufs);
        let produced = if let Some(data) = data {
            dst.put_slice(data);
            true
        } else {
            false
        };
        let consumed = input_len.saturating_sub(extra);
        if consumed != 0 {
            src.advance_to(consumed);
        }
        Ok(if produced || consumed != 0 {
            Decrypted::Progress
        } else {
            Decrypted::Pending
        })
    }

    fn peer_cert(&self) -> Option<Vec<u8>> {
        let mut cert: *mut CERT_CONTEXT = ptr::null_mut();
        let status = unsafe {
            QueryContextAttributesW(
                &raw const self.ctxt,
                SECPKG_ATTR_REMOTE_CERT_CONTEXT,
                (&raw mut cert).cast(),
            )
        };
        if status != SEC_E_OK || cert.is_null() {
            return None;
        }

        let duplicated = unsafe { CertDuplicateCertificateContext(cert) };
        unsafe {
            CertFreeCertificateContext(cert);
        }
        if duplicated.is_null() {
            return None;
        }

        let bytes = unsafe {
            let cert = &*duplicated;
            slice::from_raw_parts(cert.pbCertEncoded, cert.cbCertEncoded as usize).to_vec()
        };
        unsafe {
            CertFreeCertificateContext(duplicated);
        }
        Some(bytes)
    }

    fn alpn_protocol(&self) -> Option<Vec<u8>> {
        let mut proto = unsafe { mem::zeroed::<SecPkgContext_ApplicationProtocol>() };
        let status = unsafe {
            QueryContextAttributesW(
                &raw const self.ctxt,
                SECPKG_ATTR_APPLICATION_PROTOCOL,
                (&raw mut proto).cast(),
            )
        };
        if status == SEC_E_OK
            && proto.ProtoNegoStatus == SecApplicationProtocolNegotiationStatus_Success
            && proto.ProtocolIdSize != 0
        {
            Some(proto.ProtocolId[..proto.ProtocolIdSize as usize].to_vec())
        } else {
            None
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            if self.have_ctxt {
                DeleteSecurityContext(&raw const self.ctxt);
            }
            FreeCredentialsHandle(&raw const self.cred);
        }
    }
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
        // wait for the peer's close_notify, unless the peer already closed
        // the connection and it is never going to arrive
        if inner.state == State::Closed && !inner.peer_closed && !buf.io().is_read_eof() {
            Ok(Poll::Pending)
        } else {
            Ok(Poll::Ready(()))
        }
    }

    fn process_read_buf(&self, rb: &FilterBuf<'_>) -> io::Result<()> {
        let inner = self.inner_mut();
        if inner.state == State::Failed {
            return Ok(());
        }
        if inner.state == State::Handshaking {
            loop {
                let state = rb.with_write_buffers(|_, dst| {
                    let len = dst.len();
                    rb.with_read_src(|src| inner.ctx.handshake_step(src.as_mut(), dst))
                        .or_else(|err| {
                            if dst.len() == len {
                                Err(err)
                            } else {
                                // keep the io open until connect() flushes the alert
                                inner.state = State::Failed;
                                inner.error = Some(err);
                                Ok(HandshakeState::NeedRead)
                            }
                        })
                })?;
                match state {
                    HandshakeState::Done => break,
                    HandshakeState::NeedRead => return Ok(()),
                    HandshakeState::Continue => {}
                }
            }
            inner.state = State::Streaming;
        }

        let closed = rb.with_read_buffers(|r_src, r_dst| -> io::Result<bool> {
            if let Some(src) = r_src {
                if inner.peer_closed {
                    src.clear();
                    return Ok(false);
                }
                while !src.is_empty() {
                    match inner.ctx.decrypt(src, r_dst)? {
                        Decrypted::Progress => {}
                        Decrypted::Pending => break,
                        Decrypted::Closed => {
                            inner.peer_closed = true;
                            return Ok(true);
                        }
                    }
                }
            }
            Ok(false)
        })?;
        if closed {
            // peer sent close_notify, start graceful shutdown
            rb.io().close();
        }
        Ok(())
    }

    fn process_write_buf(&self, wb: &FilterBuf<'_>) -> io::Result<()> {
        let inner = self.inner_mut();
        if inner.state != State::Streaming {
            return Ok(());
        }

        wb.with_write_buffers(|w_src, w_dst| {
            while let Some(mut page) = w_src.take() {
                let written = inner.ctx.encrypt(&page, w_dst)?;
                page.advance_to(written);
                w_src.prepend(page);
                if written == 0 {
                    break;
                }
            }
            Ok(())
        })
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

pub async fn connect<F: Filter>(
    io: Io<F>,
    domain: &str,
    config: ClientConfig,
) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    let filter = SchannelFilter {
        inner: UnsafeCell::new(Schannel {
            ctx: Context::new(domain, &config)?,
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
        if io.read_notify().await?.is_none() {
            eof = true;
        }
        io.flush(false).await?;

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
    }
}

const ISC_FLAGS: u32 = ISC_REQ_SEQUENCE_DETECT
    | ISC_REQ_REPLAY_DETECT
    | ISC_REQ_CONFIDENTIALITY
    | ISC_REQ_ALLOCATE_MEMORY
    | ISC_REQ_EXTENDED_ERROR
    | ISC_REQ_STREAM;

/// Moves a token allocated by `InitializeSecurityContextW` to `output`.
fn take_token(buf: &SecBuffer, output: &mut ntex_bytes::BytePages) {
    if !buf.pvBuffer.is_null() {
        if buf.cbBuffer != 0 {
            let token =
                unsafe { slice::from_raw_parts(buf.pvBuffer.cast::<u8>(), buf.cbBuffer as usize) };
            output.put_slice(token);
        }
        unsafe {
            FreeContextBuffer(buf.pvBuffer);
        }
    }
}

/// Locate the decrypted payload and the unprocessed input length by buffer type,
/// `DecryptMessage` does not guarantee the position of output buffers.
fn decrypted_parts(bufs: &[SecBuffer]) -> (Option<&[u8]>, usize) {
    let data = bufs
        .iter()
        .find(|buf| {
            buf.BufferType == SECBUFFER_DATA && buf.cbBuffer != 0 && !buf.pvBuffer.is_null()
        })
        .map(|buf| unsafe {
            slice::from_raw_parts(buf.pvBuffer.cast::<u8>(), buf.cbBuffer as usize)
        });
    let extra = bufs
        .iter()
        .find(|buf| buf.BufferType == SECBUFFER_EXTRA)
        .map_or(0, |buf| buf.cbBuffer as usize);
    (data, extra)
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

    #[test]
    fn test_decrypted_parts_by_type() {
        let mut payload = *b"hello";
        let buf = |ty, len, ptr: *mut u8| SecBuffer {
            cbBuffer: len,
            BufferType: ty,
            pvBuffer: ptr.cast(),
        };
        let bufs = [
            buf(SECBUFFER_STREAM_HEADER, 13, ptr::null_mut()),
            buf(SECBUFFER_EXTRA, 7, ptr::null_mut()),
            buf(SECBUFFER_STREAM_TRAILER, 16, ptr::null_mut()),
            buf(SECBUFFER_DATA, 5, payload.as_mut_ptr()),
        ];
        let (data, extra) = decrypted_parts(&bufs);
        assert_eq!(data, Some(&b"hello"[..]));
        assert_eq!(extra, 7);

        let bufs = [
            buf(SECBUFFER_STREAM_HEADER, 13, ptr::null_mut()),
            buf(SECBUFFER_DATA, 0, payload.as_mut_ptr()),
            buf(SECBUFFER_STREAM_TRAILER, 16, ptr::null_mut()),
            buf(SECBUFFER_EMPTY, 0, ptr::null_mut()),
        ];
        assert_eq!(decrypted_parts(&bufs), (None, 0));
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
