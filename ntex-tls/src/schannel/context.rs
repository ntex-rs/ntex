//! Schannel security context.
use std::sync::Arc;
use std::{cmp, io, mem, ptr, slice};

use ntex_bytes::{BufMut, BytesMut};
use windows_sys::Win32::Foundation::{
    CRYPT_E_REVOKED, SEC_E_CERT_EXPIRED, SEC_E_CERT_UNKNOWN, SEC_E_INCOMPLETE_MESSAGE, SEC_E_OK,
    SEC_E_UNTRUSTED_ROOT, SEC_E_WRONG_PRINCIPAL, SEC_I_CONTEXT_EXPIRED, SEC_I_CONTINUE_NEEDED,
    SEC_I_INCOMPLETE_CREDENTIALS, SEC_I_RENEGOTIATE,
};
use windows_sys::Win32::Security::Authentication::Identity::{
    ASC_REQ_ALLOCATE_MEMORY, ASC_REQ_CONFIDENTIALITY, ASC_REQ_EXTENDED_ERROR, ASC_REQ_MUTUAL_AUTH,
    ASC_REQ_REPLAY_DETECT, ASC_REQ_SEQUENCE_DETECT, ASC_REQ_STREAM, AcceptSecurityContext,
    ApplyControlToken, DecryptMessage, DeleteSecurityContext, EncryptMessage, FreeContextBuffer,
    ISC_REQ_ALLOCATE_MEMORY, ISC_REQ_CONFIDENTIALITY, ISC_REQ_EXTENDED_ERROR,
    ISC_REQ_REPLAY_DETECT, ISC_REQ_SEQUENCE_DETECT, ISC_REQ_STREAM, InitializeSecurityContextW,
    QueryContextAttributesW, SCHANNEL_ALERT, SCHANNEL_ALERT_TOKEN, SCHANNEL_SHUTDOWN,
    SECBUFFER_APPLICATION_PROTOCOLS, SECBUFFER_DATA, SECBUFFER_EMPTY, SECBUFFER_EXTRA,
    SECBUFFER_STREAM_HEADER, SECBUFFER_STREAM_TRAILER, SECBUFFER_TOKEN, SECBUFFER_VERSION,
    SECPKG_ATTR_APPLICATION_PROTOCOL, SECPKG_ATTR_REMOTE_CERT_CONTEXT, SECPKG_ATTR_STREAM_SIZES,
    SECURITY_NATIVE_DREP, SecApplicationProtocolNegotiationStatus_Success, SecBuffer,
    SecBufferDesc, SecPkgContext_ApplicationProtocol, SecPkgContext_StreamSizes,
    SslGetServerIdentity, TLS1_ALERT_BAD_CERTIFICATE, TLS1_ALERT_CERTIFICATE_EXPIRED,
    TLS1_ALERT_CERTIFICATE_REVOKED, TLS1_ALERT_FATAL, TLS1_ALERT_HANDSHAKE_FAILURE,
    TLS1_ALERT_UNKNOWN_CA,
};
use windows_sys::Win32::Security::Credentials::SecHandle;
use windows_sys::Win32::Security::Cryptography::{CERT_CONTEXT, CertFreeCertificateContext};

use super::{ClientConfig, Credentials, ServerConfig, sspi_error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Decrypted {
    Progress,
    /// Needs more input
    Pending,
    /// Peer sent `close_notify`
    Closed,
    /// Post-handshake message, the context must be driven by ISC or ASC
    Renegotiate,
}

pub(super) struct Context {
    cred: Arc<Credentials>,
    ctxt: SecHandle,
    have_ctxt: bool,
    role: Role,
    alpn: Option<Arc<[u8]>>,
    sizes: Option<SecPkgContext_StreamSizes>,
    /// The server asked for a client certificate
    cert_requested: bool,
    /// Server name requested by the client
    pub(super) servername: Option<String>,
    /// Handshake payload of the consumed `ClientHello` records, until the
    /// `ClientHello` is complete
    hello: Option<Vec<u8>>,
}

#[derive(Debug)]
enum Role {
    /// Client with the wide target name
    Client(Vec<u16>),
    /// Server with `AcceptSecurityContext` flags
    Server(u32),
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("have_ctxt", &self.have_ctxt)
            .field("server", &self.is_server())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HandshakeState {
    Done,
    NeedRead,
    /// More handshake input is already buffered, call again without reading.
    Continue,
}

impl Context {
    pub(super) fn client(domain: &str, config: &ClientConfig) -> io::Result<Self> {
        let target = domain.encode_utf16().chain(Some(0)).collect();
        Ok(Self::new(
            config.credentials()?,
            Role::Client(target),
            config.alpn.clone(),
        ))
    }

    pub(super) fn server(config: &ServerConfig) -> Self {
        let mut flags = ASC_FLAGS;
        if config.request_client_cert {
            flags |= ASC_REQ_MUTUAL_AUTH;
        }
        let mut ctx = Self::new(
            config.cred.clone(),
            Role::Server(flags),
            config.alpn.clone(),
        );
        ctx.hello = Some(Vec::new());
        ctx
    }

    fn new(cred: Arc<Credentials>, role: Role, alpn: Option<Arc<[u8]>>) -> Self {
        Self {
            cred,
            ctxt: unsafe { mem::zeroed::<SecHandle>() },
            have_ctxt: false,
            role,
            alpn,
            sizes: None,
            cert_requested: false,
            servername: None,
            hello: None,
        }
    }

    fn is_server(&self) -> bool {
        matches!(self.role, Role::Server(_))
    }

    pub(super) fn handshake_step(
        &mut self,
        input: Option<&mut BytesMut>,
        output: &mut ntex_bytes::BytePages,
    ) -> io::Result<HandshakeState> {
        if self.is_server() {
            // the server waits for the client's messages
            if input.as_ref().is_none_or(|src| src.is_empty()) {
                return Ok(HandshakeState::NeedRead);
            }
        }

        let mut in_bufs = [EMPTY_BUFFER; 3];
        if let Some(src) = input.as_ref().filter(|src| !src.is_empty()) {
            in_bufs[0] = sec_buffer(
                SECBUFFER_TOKEN,
                buffer_len(src.len())?,
                src.as_ptr().cast_mut().cast(),
            );
        }
        // ALPN is negotiated by the ClientHello, the client sends it with the
        // first call, the server needs it until the ClientHello is processed,
        // which may span several records
        let pass_alpn = if self.is_server() {
            self.sizes.is_none()
        } else {
            !self.have_ctxt
        };
        if pass_alpn && let Some(alpn) = &self.alpn {
            in_bufs[2] = sec_buffer(
                SECBUFFER_APPLICATION_PROTOCOLS,
                buffer_len(alpn.len())?,
                alpn.as_ptr().cast_mut().cast(),
            );
        }
        let in_desc = buffer_desc(&mut in_bufs);
        let (status, has_token) = self.step(&raw const in_desc, output);

        if status == SEC_I_INCOMPLETE_CREDENTIALS && !mem::replace(&mut self.cert_requested, true) {
            // the server asks for a client certificate, the same input
            // continues the handshake without one
            return Ok(HandshakeState::Continue);
        }
        if status == SEC_E_INCOMPLETE_MESSAGE {
            return Ok(HandshakeState::NeedRead);
        }
        if status != SEC_E_OK && status != SEC_I_CONTINUE_NEEDED {
            if !has_token {
                // e.g. certificate validation fails without an alert, best effort
                let _ = self.fatal_alert(status, output);
            }
            return Err(sspi_error(self.step_name(), status));
        }

        let mut consumed = 0;
        let mut extra = 0;
        if let Some(src) = input {
            extra = extra_len(&in_bufs);
            consumed = src.len().saturating_sub(extra);
            if consumed != 0 {
                self.client_hello(&src[..consumed]);
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

    /// Collects the `ClientHello` from the records consumed by the server and
    /// reads the requested server name once it is complete.
    fn client_hello(&mut self, mut records: &[u8]) {
        let Some(hello) = &mut self.hello else {
            return;
        };
        while let [content_type, _, _, len_hi, len_lo, rest @ ..] = records {
            let len = usize::from(u16::from_be_bytes([*len_hi, *len_lo]));
            if *content_type != CONTENT_HANDSHAKE || rest.len() < len {
                break;
            }
            hello.extend_from_slice(&rest[..len]);
            records = &rest[len..];
        }

        // handshake message header, type and 24-bit length
        let Some(&[msg_type, a, b, c]) = hello.get(..4) else {
            return;
        };
        let size = 4 + (usize::from(a) << 16 | usize::from(b) << 8 | usize::from(c));
        if hello.len() >= size {
            if msg_type == HANDSHAKE_CLIENT_HELLO
                && let Ok(len) = u16::try_from(size)
            {
                let mut record = Vec::with_capacity(5 + size);
                record.extend_from_slice(&[CONTENT_HANDSHAKE, 3, 1]);
                record.extend_from_slice(&len.to_be_bytes());
                record.extend_from_slice(&hello[..size]);
                self.servername = client_hello_servername(&record);
            }
            self.hello = None;
        } else if hello.len() > MAX_CLIENT_HELLO {
            self.hello = None;
        }
    }

    /// Calls `InitializeSecurityContextW` or `AcceptSecurityContext` and
    /// appends the output token to `output`.
    ///
    /// Returns the status and whether a token was produced.
    fn step(
        &mut self,
        input: *const SecBufferDesc,
        output: &mut ntex_bytes::BytePages,
    ) -> (windows_sys::core::HRESULT, bool) {
        let mut out_buf = sec_buffer(SECBUFFER_TOKEN, 0, ptr::null_mut());
        let mut out_desc = buffer_desc(slice::from_mut(&mut out_buf));
        let mut attrs = 0u32;
        let mut expiry = 0i64;
        let ctxt = if self.have_ctxt {
            &raw const self.ctxt
        } else {
            ptr::null()
        };
        let status = unsafe {
            match &self.role {
                Role::Client(target) => InitializeSecurityContextW(
                    &raw const self.cred.handle,
                    ctxt,
                    target.as_ptr(),
                    ISC_FLAGS,
                    0,
                    SECURITY_NATIVE_DREP,
                    input,
                    0,
                    &raw mut self.ctxt,
                    &raw mut out_desc,
                    &raw mut attrs,
                    &raw mut expiry,
                ),
                Role::Server(flags) => AcceptSecurityContext(
                    &raw const self.cred.handle,
                    ctxt,
                    input,
                    *flags,
                    SECURITY_NATIVE_DREP,
                    &raw mut self.ctxt,
                    &raw mut out_desc,
                    &raw mut attrs,
                    &raw mut expiry,
                ),
            }
        };
        // the server context is not created until the ClientHello is complete
        self.have_ctxt |= self.ctxt.dwLower != 0 || self.ctxt.dwUpper != 0;
        let has_token = !out_buf.pvBuffer.is_null() && out_buf.cbBuffer != 0;
        take_token(&out_buf, output);
        (status, has_token)
    }

    fn step_name(&self) -> &'static str {
        if self.is_server() {
            "AcceptSecurityContext"
        } else {
            "InitializeSecurityContextW"
        }
    }

    /// Generates a `close_notify` alert and appends it to `output`.
    pub(super) fn close_notify(&mut self, output: &mut ntex_bytes::BytePages) -> io::Result<()> {
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
        let mut in_buf = sec_buffer(
            SECBUFFER_TOKEN,
            u32::try_from(mem::size_of::<T>()).expect("control token size fits u32"),
            ptr::from_mut(token).cast(),
        );
        let in_desc = buffer_desc(slice::from_mut(&mut in_buf));
        let status = unsafe { ApplyControlToken(&raw const self.ctxt, &raw const in_desc) };
        if status != SEC_E_OK {
            return Err(sspi_error(name, status));
        }

        let (status, _) = self.step(ptr::null(), output);
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

    /// Encrypts all pending plaintext pages from `src` into `dst`.
    pub(super) fn encrypt_pages(
        &mut self,
        src: &mut ntex_bytes::BytePages,
        dst: &mut ntex_bytes::BytePages,
    ) -> io::Result<()> {
        while let Some(mut page) = src.take() {
            let written = self.encrypt(&page, dst)?;
            page.advance_to(written);
            src.prepend(page);
            if written == 0 {
                break;
            }
        }
        Ok(())
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

        // encrypt in place into the current page, allocating one if needed;
        // a full page is pushed out by the next write to `dst`
        let chunk = dst.chunk_mut();
        let avail = chunk.len();
        if avail >= overhead + cmp::min(len, MIN_RECORD) {
            let len = cmp::min(len, avail - overhead);
            let tls_len = self.encrypt_into(&src[..len], chunk.as_mut_ptr(), sizes)?;
            unsafe { dst.advance_mut(tls_len) };
            return Ok(len);
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
            sec_buffer(SECBUFFER_STREAM_HEADER, sizes.cbHeader, frame.cast()),
            sec_buffer(
                SECBUFFER_DATA,
                u32::try_from(len).expect("TLS message length fits u32"),
                unsafe { frame.add(header_len).cast() },
            ),
            sec_buffer(SECBUFFER_STREAM_TRAILER, sizes.cbTrailer, unsafe {
                frame.add(header_len + len).cast()
            }),
            EMPTY_BUFFER,
        ];
        let desc = buffer_desc(&mut bufs);
        let status = unsafe { EncryptMessage(&raw const self.ctxt, 0, &raw const desc, 0) };
        if status != SEC_E_OK {
            return Err(sspi_error("EncryptMessage", status));
        }
        // header, data and trailer
        Ok(bufs[..3].iter().map(|buf| buf.cbBuffer as usize).sum())
    }

    pub(super) fn decrypt(
        &mut self,
        src: &mut BytesMut,
        dst: &mut BytesMut,
    ) -> io::Result<Decrypted> {
        if src.is_empty() {
            return Ok(Decrypted::Pending);
        }

        let input_len = src.len();
        let mut bufs = [
            sec_buffer(
                SECBUFFER_DATA,
                buffer_len(input_len)?,
                src.as_mut_ptr().cast(),
            ),
            EMPTY_BUFFER,
            EMPTY_BUFFER,
            EMPTY_BUFFER,
        ];
        let desc = buffer_desc(&mut bufs);
        let mut qop = 0u32;
        let status =
            unsafe { DecryptMessage(&raw const self.ctxt, &raw const desc, 0, &raw mut qop) };

        match status {
            SEC_E_OK | SEC_I_RENEGOTIATE => {}
            SEC_E_INCOMPLETE_MESSAGE => return Ok(Decrypted::Pending),
            SEC_I_CONTEXT_EXPIRED => {
                // peer sent close_notify, data after it is ignored
                src.clear();
                return Ok(Decrypted::Closed);
            }
            _ => return Err(sspi_error("DecryptMessage", status)),
        }

        let (data, extra) = decrypted_parts(&bufs);
        if let Some(data) = data {
            dst.put_slice(data);
        }
        let produced = data.is_some();
        let consumed = input_len.saturating_sub(extra);
        if consumed != 0 {
            src.advance_to(consumed);
        }
        Ok(if status == SEC_I_RENEGOTIATE {
            // a post-handshake message (TLS 1.3 session ticket, key update)
            // or a TLS 1.2 renegotiation, the rest of the input goes to ISC or ASC
            Decrypted::Renegotiate
        } else if produced || consumed != 0 {
            Decrypted::Progress
        } else {
            Decrypted::Pending
        })
    }

    pub(super) fn peer_cert(&self) -> Option<Vec<u8>> {
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

        // the context is owned by the caller
        unsafe {
            let bytes =
                slice::from_raw_parts((*cert).pbCertEncoded, (*cert).cbCertEncoded as usize)
                    .to_vec();
            CertFreeCertificateContext(cert);
            Some(bytes)
        }
    }

    pub(super) fn alpn_protocol(&self) -> Option<Vec<u8>> {
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
        }
    }
}

const ISC_FLAGS: u32 = ISC_REQ_SEQUENCE_DETECT
    | ISC_REQ_REPLAY_DETECT
    | ISC_REQ_CONFIDENTIALITY
    | ISC_REQ_ALLOCATE_MEMORY
    | ISC_REQ_EXTENDED_ERROR
    | ISC_REQ_STREAM;

const ASC_FLAGS: u32 = ASC_REQ_SEQUENCE_DETECT
    | ASC_REQ_REPLAY_DETECT
    | ASC_REQ_CONFIDENTIALITY
    | ASC_REQ_ALLOCATE_MEMORY
    | ASC_REQ_EXTENDED_ERROR
    | ASC_REQ_STREAM;

const CONTENT_HANDSHAKE: u8 = 22;
const HANDSHAKE_CLIENT_HELLO: u8 = 1;
/// The largest `ClientHello` searched for the server name
const MAX_CLIENT_HELLO: usize = 64 * 1024;

/// Server name indication of a `ClientHello` record.
fn client_hello_servername(src: &[u8]) -> Option<String> {
    let mut name = ptr::null_mut();
    let mut len = 0u32;
    let status = unsafe {
        SslGetServerIdentity(
            src.as_ptr(),
            u32::try_from(src.len()).ok()?,
            &raw mut name,
            &raw mut len,
            0,
        )
    };
    if status != SEC_E_OK || name.is_null() || len == 0 {
        return None;
    }
    // points into `src`
    let name = unsafe { slice::from_raw_parts(name, len as usize) };
    std::str::from_utf8(name).ok().map(str::to_owned)
}

/// Moves a token allocated by `InitializeSecurityContextW` or
/// `AcceptSecurityContext` to `output`.
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
    (data, extra_len(bufs))
}

/// Unprocessed input length reported by a `SECBUFFER_EXTRA` buffer.
fn extra_len(bufs: &[SecBuffer]) -> usize {
    bufs.iter()
        .find(|buf| buf.BufferType == SECBUFFER_EXTRA)
        .map_or(0, |buf| buf.cbBuffer as usize)
}

const EMPTY_BUFFER: SecBuffer = sec_buffer(SECBUFFER_EMPTY, 0, ptr::null_mut());

const fn sec_buffer(ty: u32, len: u32, ptr: *mut std::ffi::c_void) -> SecBuffer {
    SecBuffer {
        cbBuffer: len,
        BufferType: ty,
        pvBuffer: ptr,
    }
}

/// Describes `bufs`, the descriptor must not outlive them.
fn buffer_desc(bufs: &mut [SecBuffer]) -> SecBufferDesc {
    SecBufferDesc {
        ulVersion: SECBUFFER_VERSION,
        cBuffers: u32::try_from(bufs.len()).expect("SecBuffer count fits u32"),
        pBuffers: bufs.as_mut_ptr(),
    }
}

fn buffer_len(len: usize) -> io::Result<u32> {
    u32::try_from(len).map_err(|_| io::Error::other("TLS buffer is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decrypted_parts_by_type() {
        let mut payload = *b"hello";
        let buf = |ty, len, ptr: *mut u8| sec_buffer(ty, len, ptr.cast());
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
}
