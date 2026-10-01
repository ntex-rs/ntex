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

#[allow(clippy::struct_excessive_bools)]
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
    /// The client's handshake records are plaintext and are repacked before
    /// they are passed to the server
    coalesce: bool,
    /// Protocol negotiated by ALPN
    negotiated_alpn: Option<Vec<u8>>,
    /// The handshake waits for the peer's messages, Schannel must not be
    /// called without input, e.g. `DecryptMessage` fails after that during
    /// a TLS 1.2 renegotiation
    awaiting_peer: bool,
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
        Self::new(
            config.cred.clone(),
            Role::Server(flags),
            config.alpn.clone(),
        )
    }

    fn new(cred: Arc<Credentials>, role: Role, alpn: Option<Arc<[u8]>>) -> Self {
        Self {
            cred,
            ctxt: unsafe { mem::zeroed::<SecHandle>() },
            have_ctxt: false,
            coalesce: matches!(role, Role::Server(_)),
            role,
            alpn,
            sizes: None,
            cert_requested: false,
            servername: None,
            negotiated_alpn: None,
            awaiting_peer: false,
        }
    }

    fn is_server(&self) -> bool {
        matches!(self.role, Role::Server(_))
    }

    pub(super) fn handshake_step(
        &mut self,
        mut input: Option<&mut BytesMut>,
        output: &mut ntex_bytes::BytePages,
    ) -> io::Result<HandshakeState> {
        // input buffered after the records passed to the server
        let mut held = 0;
        let len = match input.as_deref_mut() {
            // a post-handshake exchange must not consume application data
            // that follows its messages
            Some(src) if self.sizes.is_some() => handshake_records_len(src),
            Some(src) if self.coalesce => {
                let Some(len) = coalesce_handshake_records(src) else {
                    return Ok(HandshakeState::NeedRead);
                };
                if !self.have_ctxt {
                    // the context is created once the ClientHello is complete
                    self.servername = client_hello_servername(&src[..len]);
                }
                held = src.len() - len;
                len
            }
            Some(src) => src.len(),
            None => 0,
        };
        // the server waits for the client's messages
        if len == 0 && (self.is_server() || self.awaiting_peer) {
            return Ok(HandshakeState::NeedRead);
        }

        let mut in_bufs = [EMPTY_BUFFER; 3];
        if let Some(src) = input.as_ref().filter(|_| len != 0) {
            in_bufs[0] = sec_buffer(
                SECBUFFER_TOKEN,
                buffer_len(len)?,
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
        self.awaiting_peer = status == SEC_I_CONTINUE_NEEDED || status == SEC_E_INCOMPLETE_MESSAGE;

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
            consumed = len.saturating_sub(extra);
            if consumed != 0 {
                // the following handshake records of TLS 1.2 are encrypted
                self.coalesce &= !records(&src[..consumed])
                    .any(|(content_type, _)| content_type == CONTENT_CHANGE_CIPHER_SPEC);
                src.advance_to(consumed);
            }
        }

        if status == SEC_E_OK {
            if self.sizes.is_none() {
                // Schannel does not report the protocol after a post-handshake
                // message, e.g. a TLS 1.3 session ticket
                self.negotiated_alpn = self.query_alpn_protocol();
            }
            self.query_stream_sizes()?;
            Ok(HandshakeState::Done)
        } else if consumed != 0 && (extra != 0 || held != 0) {
            Ok(HandshakeState::Continue)
        } else {
            Ok(HandshakeState::NeedRead)
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
            dwAlertNumber: alert_number(status),
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
        // one record at a time, a TLS 1.2 `HelloRequest` followed by more
        // records is not consumed and application data after it cannot be
        // decrypted during the renegotiation
        let record_len = match src.get(..5) {
            Some(&[_, _, _, hi, lo]) => {
                cmp::min(5 + usize::from(u16::from_be_bytes([hi, lo])), input_len)
            }
            _ => input_len,
        };
        let mut bufs = [
            sec_buffer(
                SECBUFFER_DATA,
                buffer_len(record_len)?,
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
            SEC_E_OK => {}
            SEC_I_RENEGOTIATE => self.awaiting_peer = false,
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
        let consumed = record_len.saturating_sub(extra);
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

    pub(super) fn alpn_protocol(&self) -> Option<&[u8]> {
        self.negotiated_alpn.as_deref()
    }

    fn query_alpn_protocol(&self) -> Option<Vec<u8>> {
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

const CONTENT_CHANGE_CIPHER_SPEC: u8 = 20;
const CONTENT_HANDSHAKE: u8 = 22;
/// The largest plaintext record payload
const MAX_RECORD: usize = 16 * 1024;
/// The largest handshake data held back until a message is complete
const MAX_HELD_HANDSHAKE: usize = 64 * 1024;

/// Alert sent for a failed handshake.
fn alert_number(status: windows_sys::core::HRESULT) -> u32 {
    match status {
        SEC_E_UNTRUSTED_ROOT => TLS1_ALERT_UNKNOWN_CA,
        SEC_E_CERT_EXPIRED => TLS1_ALERT_CERTIFICATE_EXPIRED,
        CRYPT_E_REVOKED => TLS1_ALERT_CERTIFICATE_REVOKED,
        SEC_E_WRONG_PRINCIPAL | SEC_E_CERT_UNKNOWN => TLS1_ALERT_BAD_CERTIFICATE,
        _ => TLS1_ALERT_HANDSHAKE_FAILURE,
    }
}

/// Server name indication of the `ClientHello` in the leading handshake
/// records of `src`.
fn client_hello_servername(src: &[u8]) -> Option<String> {
    // the message is passed in a single record
    let mut record = vec![CONTENT_HANDSHAKE, 3, 1, 0, 0];
    for (_, payload) in records(src).take_while(|(ty, _)| *ty == CONTENT_HANDSHAKE) {
        record.extend_from_slice(payload);
    }
    let len = u16::try_from(record.len() - 5).ok()?;
    record[3..5].copy_from_slice(&len.to_be_bytes());

    let mut name = ptr::null_mut();
    let mut len = 0u32;
    let status = unsafe {
        SslGetServerIdentity(
            record.as_ptr(),
            u32::try_from(record.len()).ok()?,
            &raw mut name,
            &raw mut len,
            0,
        )
    };
    if status != SEC_E_OK || name.is_null() || len == 0 {
        return None;
    }
    // points into `record`
    let name = unsafe { slice::from_raw_parts(name, len as usize) };
    std::str::from_utf8(name).ok().map(str::to_owned)
}

/// Content type and payload of the complete records at the start of `src`.
fn records(mut src: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        let [content_type, _, _, hi, lo, rest @ ..] = src else {
            return None;
        };
        let len = usize::from(u16::from_be_bytes([*hi, *lo]));
        let payload = rest.get(..len)?;
        src = &rest[len..];
        Some((*content_type, payload))
    })
}

/// Length of the leading handshake and change cipher spec records of `src`,
/// including an incomplete one.
pub(super) fn handshake_records_len(src: &[u8]) -> usize {
    let is_handshake = |ty: u8| ty == CONTENT_HANDSHAKE || ty == CONTENT_CHANGE_CIPHER_SPEC;
    let len = records(src)
        .take_while(|(ty, _)| is_handshake(*ty))
        .map(|(_, payload)| 5 + payload.len())
        .sum();
    if src.get(len).is_some_and(|ty| is_handshake(*ty)) {
        src.len()
    } else {
        len
    }
}

/// Repacks the complete handshake messages in the leading plaintext
/// handshake records of `src` into as few records as possible, returns the
/// length of these records, or `None` until a message is complete.
///
/// Windows Server 2025 (build 26100) fails with `SEC_E_UNSUPPORTED_FUNCTION`
/// if a `ClientHello` spread over several records is passed in pieces.
fn coalesce_handshake_records(src: &mut BytesMut) -> Option<usize> {
    let mut rest = &src[..];
    let mut version = [3, 1];
    let mut records = 0;
    let mut payload = Vec::new();
    while let [CONTENT_HANDSHAKE, major, minor, hi, lo, data @ ..] = rest {
        let len = usize::from(u16::from_be_bytes([*hi, *lo]));
        if data.len() < len {
            break;
        }
        if records == 0 {
            version = [*major, *minor];
        }
        payload.extend_from_slice(&data[..len]);
        rest = &data[len..];
        records += 1;
    }

    // handshake messages, type and 24-bit length
    let mut complete = 0;
    while let Some(&[_, a, b, c]) = payload.get(complete..complete + 4) {
        let size = 4 + (usize::from(a) << 16 | usize::from(b) << 8 | usize::from(c));
        if payload.len() - complete < size {
            break;
        }
        complete += size;
    }

    if records == 0 || payload.len() > MAX_HELD_HANDSHAKE {
        return Some(src.len());
    }
    if complete == 0 {
        // anything else than the rest of the message is reported by Schannel
        let waiting = rest.first().is_none_or(|&ty| ty == CONTENT_HANDSHAKE);
        return if waiting { None } else { Some(src.len()) };
    }
    if records == 1 && complete == payload.len() {
        return Some(src.len());
    }

    let mut out = Vec::with_capacity(src.len());
    let mut len = 0;
    for part in [&payload[..complete], &payload[complete..]] {
        for chunk in part.chunks(MAX_RECORD) {
            out.extend_from_slice(&[CONTENT_HANDSHAKE, version[0], version[1]]);
            out.extend_from_slice(&u16::try_from(chunk.len()).unwrap().to_be_bytes());
            out.extend_from_slice(chunk);
        }
        if len == 0 {
            len = out.len();
        }
    }
    out.extend_from_slice(rest);
    src.clear();
    src.extend_from_slice(&out);
    Some(len)
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

    const CONTENT_APPLICATION_DATA: u8 = 23;
    const HANDSHAKE_CLIENT_HELLO: u8 = 1;

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

    fn record(content_type: u8, payload: &[u8]) -> Vec<u8> {
        let len = u16::try_from(payload.len()).unwrap();
        let mut rec = vec![content_type, 3, 3];
        rec.extend_from_slice(&len.to_be_bytes());
        rec.extend_from_slice(payload);
        rec
    }

    fn message(msg_type: u8, len: usize) -> Vec<u8> {
        let mut msg = vec![msg_type];
        msg.extend_from_slice(&u32::try_from(len).unwrap().to_be_bytes()[1..]);
        msg.extend((0..=u8::MAX).cycle().take(len));
        msg
    }

    /// Records holding `data` split at `cuts`.
    fn fragments(data: &[u8], cuts: &[usize]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut start = 0;
        for &end in cuts.iter().chain(Some(&data.len())) {
            out.extend(record(CONTENT_HANDSHAKE, &data[start..end]));
            start = end;
        }
        out
    }

    fn coalesce(src: &[u8]) -> (Option<usize>, Vec<u8>) {
        let mut buf = BytesMut::copy_from_slice(src);
        let len = coalesce_handshake_records(&mut buf);
        (len, buf.to_vec())
    }

    fn server_context() -> Context {
        let pfx = include_bytes!("../../examples/identity.pfx");
        let cert = super::super::Certificate::from_pkcs12(pfx, "ntex").unwrap();
        Context::server(&ServerConfig::new(cert).unwrap())
    }

    /// `ClientHello` handshake message with a server name extension.
    fn client_hello_message(name: &str) -> Vec<u8> {
        let name = name.as_bytes();
        let len = u16::try_from(name.len()).unwrap();
        let mut sni = vec![0, 0];
        sni.extend_from_slice(&(len + 5).to_be_bytes());
        sni.extend_from_slice(&(len + 3).to_be_bytes());
        sni.push(0);
        sni.extend_from_slice(&len.to_be_bytes());
        sni.extend_from_slice(name);

        let mut body = vec![3, 3];
        body.extend([7; 32]);
        // no session id, one cipher suite, null compression
        body.extend([0, 0, 2, 0xc0, 0x2f, 1, 0]);
        body.extend_from_slice(&u16::try_from(sni.len()).unwrap().to_be_bytes());
        body.extend(sni);

        let mut msg = vec![HANDSHAKE_CLIENT_HELLO];
        msg.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes()[1..]);
        msg.extend(body);
        msg
    }

    #[test]
    fn test_client_hello_servername() {
        let msg = client_hello_message("example.com");
        for cuts in [&[][..], &[1], &[2], &[3], &[1, 2, 3, 4, 50]] {
            let src = fragments(&msg, cuts);
            let name = client_hello_servername(&src);
            assert_eq!(name.as_deref(), Some("example.com"), "{cuts:?}");

            // the server reads it before passing the coalesced records to Schannel
            let mut ctx = server_context();
            let mut src = BytesMut::copy_from_slice(&src);
            let mut out = ntex_bytes::BytePages::default();
            let _ = ctx.handshake_step(Some(&mut src), &mut out);
            assert_eq!(ctx.servername.as_deref(), Some("example.com"), "{cuts:?}");
        }

        let hello = record(CONTENT_HANDSHAKE, &msg);
        let mut other = msg.clone();
        other[0] = 2;
        for src in [
            // incomplete
            &hello[..10],
            // not the first record
            &[&record(CONTENT_CHANGE_CIPHER_SPEC, &[1])[..], &hello].concat(),
            // another message
            &record(CONTENT_HANDSHAKE, &other),
            // too large for a record
            &fragments(&message(HANDSHAKE_CLIENT_HELLO, 70_000), &[30_000, 60_000]),
        ] {
            assert!(client_hello_servername(src).is_none());
        }
    }

    #[test]
    fn test_alert_number() {
        for (status, alert) in [
            (SEC_E_UNTRUSTED_ROOT, TLS1_ALERT_UNKNOWN_CA),
            (SEC_E_CERT_EXPIRED, TLS1_ALERT_CERTIFICATE_EXPIRED),
            (CRYPT_E_REVOKED, TLS1_ALERT_CERTIFICATE_REVOKED),
            (SEC_E_WRONG_PRINCIPAL, TLS1_ALERT_BAD_CERTIFICATE),
            (SEC_E_CERT_UNKNOWN, TLS1_ALERT_BAD_CERTIFICATE),
            (
                windows_sys::Win32::Foundation::SEC_E_INTERNAL_ERROR,
                TLS1_ALERT_HANDSHAKE_FAILURE,
            ),
        ] {
            assert_eq!(alert_number(status), alert, "{status:#x}");
        }
    }

    #[test]
    fn test_handshake_records_len() {
        let hs = record(CONTENT_HANDSHAKE, b"hello");
        let ccs = record(CONTENT_CHANGE_CIPHER_SPEC, &[1]);
        let alert = record(21, &[2, 40]);
        let app = record(CONTENT_APPLICATION_DATA, b"data");

        assert_eq!(handshake_records_len(&[]), 0);
        assert_eq!(handshake_records_len(&app), 0);
        assert_eq!(handshake_records_len(&alert), 0);
        assert_eq!(handshake_records_len(&[&hs[..], &app].concat()), hs.len());
        assert_eq!(handshake_records_len(&[&hs[..], &alert].concat()), hs.len());
        assert_eq!(
            handshake_records_len(&[&ccs[..], &hs, &app].concat()),
            ccs.len() + hs.len()
        );
        assert_eq!(
            handshake_records_len(&[&hs[..], &app[..3]].concat()),
            hs.len()
        );

        // trailing complete, incomplete records and headers
        assert_eq!(handshake_records_len(&hs), hs.len());
        assert_eq!(handshake_records_len(&hs[..7]), 7);
        assert_eq!(handshake_records_len(&hs[..3]), 3);
        let src = [&hs[..], &ccs[..3]].concat();
        assert_eq!(handshake_records_len(&src), src.len());
    }

    #[test]
    fn test_coalesce_waits_for_complete_message() {
        let hello = message(HANDSHAKE_CLIENT_HELLO, 300);
        let src = fragments(&hello, &[64, 128, 192]);
        let full = record(CONTENT_HANDSHAKE, &hello);

        // an incomplete first record is passed as is, the rest is held back
        // until the message is complete
        for end in 1..src.len() {
            let (len, buf) = coalesce(&src[..end]);
            let expected = if end < 5 + 64 { Some(end) } else { None };
            assert_eq!(len, expected, "{end}");
            assert_eq!(buf, &src[..end]);
        }
        assert_eq!(coalesce(&src), (Some(full.len()), full.clone()));

        // following records are kept
        let mut src = src;
        let ccs = record(CONTENT_CHANGE_CIPHER_SPEC, &[1]);
        src.extend_from_slice(&ccs);
        src.extend_from_slice(&[22, 3, 3, 0]);
        let mut expected = full.clone();
        expected.extend_from_slice(&ccs);
        expected.extend_from_slice(&[22, 3, 3, 0]);
        assert_eq!(coalesce(&src), (Some(full.len()), expected));
    }

    #[test]
    fn test_coalesce_partial_next_message() {
        let mut data = message(HANDSHAKE_CLIENT_HELLO, 100);
        let next = message(16, 50);
        data.extend_from_slice(&next[..20]);
        let (len, buf) = coalesce(&fragments(&data, &[30, 90]));

        let mut expected = record(CONTENT_HANDSHAKE, &data[..104]);
        let first = expected.len();
        expected.extend(record(CONTENT_HANDSHAKE, &next[..20]));
        assert_eq!((len, buf), (Some(first), expected));

        // a single record holding a message and a partial one is split too
        let (len, buf) = coalesce(&record(CONTENT_HANDSHAKE, &data));
        assert_eq!(len, Some(first));
        assert_eq!(buf.len(), first + 25);
    }

    #[test]
    fn test_coalesce_large_message() {
        let msg = message(11, 40_000);
        let src = fragments(&msg, &[100, 20_000, 30_000]);
        let (len, buf) = coalesce(&src);
        let expected = fragments(&msg, &[MAX_RECORD, 2 * MAX_RECORD]);
        assert_eq!((len, buf), (Some(expected.len()), expected));
    }

    #[test]
    fn test_coalesce_passes_input_unchanged() {
        let hello = record(CONTENT_HANDSHAKE, &message(HANDSHAKE_CLIENT_HELLO, 50));
        let ccs = record(CONTENT_CHANGE_CIPHER_SPEC, &[1]);
        let partial = &fragments(&message(HANDSHAKE_CLIENT_HELLO, 50), &[20])[..25];
        let alert = record(21, &[2, 40]);
        for src in [
            // complete messages in single records
            hello.clone(),
            [&hello[..], &ccs, &[23, 3, 3]].concat(),
            // not a plaintext handshake record first
            [&ccs[..], &hello].concat(),
            [23, 3, 3, 0, 1, 0].to_vec(),
            // an incomplete first record
            hello[..3].to_vec(),
            // an incomplete message followed by something else
            [partial, &alert].concat(),
        ] {
            assert_eq!(coalesce(&src), (Some(src.len()), src.clone()));
        }
        // too large to hold back
        let msg = message(11, MAX_HELD_HANDSHAKE + 1);
        let src = fragments(
            &msg[..MAX_HELD_HANDSHAKE + 4],
            &[MAX_RECORD, 2 * MAX_RECORD, 3 * MAX_RECORD],
        );
        assert_eq!(coalesce(&src), (Some(src.len()), src.clone()));
    }

    #[test]
    fn test_records() {
        let hello = record(CONTENT_HANDSHAKE, &message(HANDSHAKE_CLIENT_HELLO, 50));
        let ccs = record(CONTENT_CHANGE_CIPHER_SPEC, &[1]);
        let parse = |src: &[u8]| {
            records(src)
                .map(|(ty, p)| (ty, p.len()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            parse(&[&hello[..], &ccs, &[22, 3, 3, 0]].concat()),
            [(CONTENT_HANDSHAKE, 54), (CONTENT_CHANGE_CIPHER_SPEC, 1)]
        );
        // a payload byte is not a record, an incomplete record is skipped
        assert_eq!(parse(&[22, 3, 3, 0, 1, 20, 3, 3]), [(CONTENT_HANDSHAKE, 1)]);
        assert_eq!(parse(&[22, 3, 3, 0, 2, 20]), []);
    }
}
