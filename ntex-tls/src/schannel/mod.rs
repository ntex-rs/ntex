//! An implementation of TLS streams backed by Windows Schannel.
#![cfg(windows)]

use std::{any, cell::RefCell, cmp, io, mem, ptr, slice, task::Poll};

use ntex_bytes::{BufMut, BytesMut};
use ntex_io::{Filter, FilterBuf, FilterLayer, Io, Layer, types};
use windows_sys::Win32::Foundation::{
    SEC_E_INCOMPLETE_MESSAGE, SEC_E_OK, SEC_I_CONTEXT_EXPIRED, SEC_I_CONTINUE_NEEDED,
    SEC_I_RENEGOTIATE,
};
use windows_sys::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, DecryptMessage, DeleteSecurityContext, EncryptMessage,
    FreeContextBuffer, FreeCredentialsHandle, ISC_REQ_ALLOCATE_MEMORY,
    ISC_REQ_CONFIDENTIALITY, ISC_REQ_EXTENDED_ERROR, ISC_REQ_REPLAY_DETECT,
    ISC_REQ_SEQUENCE_DETECT, ISC_REQ_STREAM, InitializeSecurityContextW,
    QueryContextAttributesW, SCH_CRED_AUTO_CRED_VALIDATION,
    SCH_CRED_MANUAL_CRED_VALIDATION, SCH_CRED_NO_SERVERNAME_CHECK, SCH_USE_STRONG_CRYPTO,
    SCHANNEL_CRED, SCHANNEL_CRED_VERSION, SECBUFFER_APPLICATION_PROTOCOLS, SECBUFFER_DATA,
    SECBUFFER_EMPTY, SECBUFFER_EXTRA, SECBUFFER_STREAM_HEADER, SECBUFFER_STREAM_TRAILER,
    SECBUFFER_TOKEN, SECBUFFER_VERSION, SECPKG_ATTR_APPLICATION_PROTOCOL,
    SECPKG_ATTR_REMOTE_CERT_CONTEXT, SECPKG_ATTR_STREAM_SIZES, SECPKG_CRED_OUTBOUND,
    SECURITY_NATIVE_DREP, SecApplicationProtocolNegotiationExt_ALPN,
    SecApplicationProtocolNegotiationStatus_Success, SecBuffer, SecBufferDesc,
    SecPkgContext_ApplicationProtocol, SecPkgContext_StreamSizes, UNISP_NAME_W,
};
use windows_sys::Win32::Security::Credentials::SecHandle;
use windows_sys::Win32::Security::Cryptography::{
    CERT_CONTEXT, CertDuplicateCertificateContext, CertFreeCertificateContext,
};

mod connect;
pub use self::connect::{
    TlsConnector, TlsConnector2, TlsConnectorService, TlsConnectorService2,
};

/// Windows Schannel client configuration.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    verify: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientConfig {
    /// Construct default Schannel client configuration.
    #[must_use]
    pub fn new() -> Self {
        Self { verify: true }
    }

    /// Accept invalid server certificates and hostnames.
    #[must_use]
    pub fn danger_accept_invalid_certs(mut self, accept_invalid_certs: bool) -> Self {
        self.verify = !accept_invalid_certs;
        self
    }
}

/// Connection's peer certificate in DER encoding.
#[derive(Clone, Debug)]
pub struct PeerCert(pub Vec<u8>);

#[derive(Debug)]
/// An implementation of TLS streams backed by Windows Schannel.
pub struct SchannelFilter {
    inner: RefCell<Schannel>,
}

#[derive(Debug)]
struct Schannel {
    ctx: Context,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Handshaking,
    Streaming,
}

struct Context {
    cred: SecHandle,
    ctxt: SecHandle,
    have_ctxt: bool,
    target: Vec<u16>,
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
            schannel_cred.dwFlags |=
                SCH_CRED_MANUAL_CRED_VALIDATION | SCH_CRED_NO_SERVERNAME_CHECK;
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

        let mut alpn = alpn_buffer();
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
                cbBuffer: u32::try_from(alpn.len())
                    .map_err(|_| io::Error::other("TLS ALPN buffer is too large"))?,
                BufferType: SECBUFFER_APPLICATION_PROTOCOLS,
                pvBuffer: alpn.as_mut_ptr().cast(),
            },
        ];
        let in_desc = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: u32::try_from(in_bufs.len())
                .expect("static SecBuffer count fits u32"),
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
                ISC_REQ_SEQUENCE_DETECT
                    | ISC_REQ_REPLAY_DETECT
                    | ISC_REQ_CONFIDENTIALITY
                    | ISC_REQ_ALLOCATE_MEMORY
                    | ISC_REQ_EXTENDED_ERROR
                    | ISC_REQ_STREAM,
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

        if !out_buf.pvBuffer.is_null() {
            if out_buf.cbBuffer != 0 {
                let token = unsafe {
                    slice::from_raw_parts(
                        out_buf.pvBuffer.cast::<u8>(),
                        out_buf.cbBuffer as usize,
                    )
                };
                output.put_slice(token);
            }
            unsafe {
                FreeContextBuffer(out_buf.pvBuffer);
            }
        }

        if status == SEC_E_INCOMPLETE_MESSAGE {
            return Ok(HandshakeState::NeedRead);
        }
        if status != SEC_E_OK && status != SEC_I_CONTINUE_NEEDED {
            return Err(sspi_error("InitializeSecurityContextW", status));
        }

        if let Some(src) = input {
            let extra = in_bufs
                .iter()
                .find(|buf| buf.BufferType == SECBUFFER_EXTRA)
                .map_or(0, |buf| buf.cbBuffer as usize);
            let consumed = input_len.saturating_sub(extra);
            if consumed != 0 {
                src.advance_to(consumed);
            }
        }

        if status == SEC_E_OK {
            self.query_stream_sizes()?;
            Ok(HandshakeState::Done)
        } else {
            Ok(HandshakeState::NeedRead)
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

    fn encrypt(
        &mut self,
        src: &[u8],
        dst: &mut ntex_bytes::BytePages,
    ) -> io::Result<usize> {
        let sizes = self.query_stream_sizes()?;
        let len = cmp::min(src.len(), sizes.cbMaximumMessage as usize);
        if len == 0 {
            return Ok(0);
        }

        let header_len = sizes.cbHeader as usize;
        let trailer_len = sizes.cbTrailer as usize;
        let mut frame = BytesMut::with_capacity(header_len + len + trailer_len);
        frame.resize(header_len + len + trailer_len, 0);
        frame[header_len..header_len + len].copy_from_slice(&src[..len]);

        let mut bufs = [
            SecBuffer {
                cbBuffer: sizes.cbHeader,
                BufferType: SECBUFFER_STREAM_HEADER,
                pvBuffer: frame.as_mut_ptr().cast(),
            },
            SecBuffer {
                cbBuffer: u32::try_from(len).expect("TLS message length fits u32"),
                BufferType: SECBUFFER_DATA,
                pvBuffer: unsafe { frame.as_mut_ptr().add(header_len).cast() },
            },
            SecBuffer {
                cbBuffer: sizes.cbTrailer,
                BufferType: SECBUFFER_STREAM_TRAILER,
                pvBuffer: unsafe { frame.as_mut_ptr().add(header_len + len).cast() },
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

        let tls_len = usize::try_from(bufs[0].cbBuffer)
            .expect("TLS header length fits usize")
            + usize::try_from(bufs[1].cbBuffer).expect("TLS data length fits usize")
            + usize::try_from(bufs[2].cbBuffer).expect("TLS trailer length fits usize");
        frame.truncate(tls_len);
        dst.append(frame);
        Ok(len)
    }

    fn decrypt(&mut self, src: &mut BytesMut, dst: &mut BytesMut) -> io::Result<bool> {
        if src.is_empty() {
            return Ok(false);
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
        let status = unsafe {
            DecryptMessage(&raw const self.ctxt, &raw const desc, 0, &raw mut qop)
        };

        match status {
            SEC_E_OK => {}
            SEC_E_INCOMPLETE_MESSAGE => return Ok(false),
            SEC_I_CONTEXT_EXPIRED => {
                src.clear();
                return Ok(false);
            }
            SEC_I_RENEGOTIATE => {
                return Err(io::Error::other("TLS renegotiation is not supported"));
            }
            _ => return Err(sspi_error("DecryptMessage", status)),
        }

        let mut produced = false;
        if bufs[1].BufferType == SECBUFFER_DATA && bufs[1].cbBuffer != 0 {
            let data = unsafe {
                slice::from_raw_parts(
                    bufs[1].pvBuffer.cast::<u8>(),
                    bufs[1].cbBuffer as usize,
                )
            };
            dst.put_slice(data);
            produced = true;
        }

        let extra = if bufs[3].BufferType == SECBUFFER_EXTRA {
            bufs[3].cbBuffer as usize
        } else {
            0
        };
        let consumed = input_len.saturating_sub(extra);
        if consumed != 0 {
            src.advance_to(consumed);
        }
        Ok(produced || consumed != 0)
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

        let inner = self.inner.borrow();
        if inner.state != State::Streaming {
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

    fn shutdown(&self, _: &FilterBuf<'_>) -> io::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }

    fn process_read_buf(&self, rb: &FilterBuf<'_>) -> io::Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.state == State::Handshaking {
            let state = rb.with_write_buffers(|_, dst| {
                rb.with_read_src(|src| inner.ctx.handshake_step(src.as_mut(), dst))
            })?;
            if state == HandshakeState::NeedRead {
                return Ok(());
            }
            inner.state = State::Streaming;
        }

        rb.with_read_buffers(|r_src, r_dst| {
            if let Some(src) = r_src {
                loop {
                    let progressed = inner.ctx.decrypt(src, r_dst)?;
                    if !progressed {
                        break;
                    }
                    if src.is_empty() {
                        break;
                    }
                }
            }
            Ok(())
        })
    }

    fn process_write_buf(&self, wb: &FilterBuf<'_>) -> io::Result<()> {
        let mut inner = self.inner.borrow_mut();
        if inner.state != State::Streaming {
            return Ok(());
        }

        wb.with_write_buffers(|w_src, w_dst| {
            while let Some(mut page) = w_src.take() {
                let written = inner.ctx.encrypt(&page, w_dst)?;
                page.advance_to(written);
                if w_src.prepend(page) {
                    break;
                }
            }
            Ok(())
        })
    }
}

impl SchannelFilter {
    fn start_handshake(&self, buf: &FilterBuf<'_>) -> io::Result<HandshakeState> {
        let mut inner = self.inner.borrow_mut();
        buf.with_write_buffers(|_, dst| {
            let state = inner.ctx.handshake_step(None, dst)?;
            if state == HandshakeState::Done {
                inner.state = State::Streaming;
            }
            Ok(state)
        })
    }

    fn is_handshaking(&self) -> bool {
        self.inner.borrow().state == State::Handshaking
    }
}

pub async fn connect<F: Filter>(
    io: Io<F>,
    domain: &str,
    config: ClientConfig,
) -> io::Result<Io<Layer<SchannelFilter, F>>> {
    let filter = SchannelFilter {
        inner: RefCell::new(Schannel {
            ctx: Context::new(domain, &config)?,
            state: State::Handshaking,
        }),
    };
    let io = io.add_filter(filter);

    let state = io.with_buf(|buf| io.filter().start_handshake(buf))??;
    io.flush(false).await?;

    if state == HandshakeState::Done {
        return Ok(io);
    }

    loop {
        io.read_notify()
            .await?
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "disconnected"))?;
        io.flush(false).await?;

        if !io.filter().is_handshaking() {
            return Ok(io);
        }
    }
}

fn alpn_buffer() -> Vec<u8> {
    // Layout for SECBUFFER_APPLICATION_PROTOCOLS:
    // u32 ProtocolListsSize, then one or more protocol lists.
    // Each protocol list is u32 negotiation extension, u16 list size, then ALPN wire list.
    const ALPN_WIRE: &[u8] = b"\x02h2\x08http/1.1";
    let list_size = u16::try_from(ALPN_WIRE.len()).expect("static ALPN list fits u16");
    let protocol_lists_size = 4u32 + 2 + u32::from(list_size);

    let mut buf = Vec::with_capacity(4 + protocol_lists_size as usize);
    buf.extend_from_slice(&protocol_lists_size.to_ne_bytes());
    buf.extend_from_slice(
        &(SecApplicationProtocolNegotiationExt_ALPN as u32).to_ne_bytes(),
    );
    buf.extend_from_slice(&list_size.to_ne_bytes());
    buf.extend_from_slice(ALPN_WIRE);
    buf
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
