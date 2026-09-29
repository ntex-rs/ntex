use std::{cell::Cell, fmt, task::Poll};

use ntex_http::header::{HeaderName, HeaderValue};
use ntex_http::{Method, StatusCode, Uri, Version, header, uri::Authority};
use ntex_httparse::{self as httparse, HeaderParsed, Status};

use super::encoder::is_bodyless;
use crate::http::config::HttpServiceConfig;
use crate::http::message::{ConnectionType, ResponseHead};
use crate::http::{HeaderItem, error::DecodeError, header::HeaderMap, request::Request};
use crate::util::{ByteString, Bytes, BytesMut};
use crate::{codec::Decoder, service::cfg::Cfg};

/// Incoming message decoder
pub(crate) struct MessageDecoder<T: MessageType> {
    hdrs: Cell<bool>,
    inner: Cell<Option<Box<Inner<T>>>>,
}

struct Inner<T: MessageType> {
    st: State,
    val: Option<T>,
    hdr: httparse::Header,
    hdr_st: httparse::State,
    cfg: Cfg<HttpServiceConfig>,
    consumed: usize,
    /// number of parsed header lines of the current message
    headers: u16,
    /// start line parser, resumed after a partial start line
    line: T::Parser,
    line_st: httparse::State,
}

#[derive(Debug, PartialEq, Eq)]
/// The payload framing and decoder selected for an incoming HTTP/1 message.
pub enum PayloadType {
    /// The message has no payload.
    None,
    /// The message has an HTTP body.
    ///
    /// Depending on the message headers and version, the decoder may use a
    /// fixed length, chunked transfer coding, or connection close as the body
    /// delimiter.
    Payload(PayloadDecoder),
    /// The message switches the connection away from HTTP framing.
    ///
    /// Subsequent bytes belong to the upgraded protocol or tunnel.
    Stream(PayloadDecoder),
}

impl<T: MessageType> Default for MessageDecoder<T> {
    fn default() -> Self {
        MessageDecoder::new(Cfg::default())
    }
}

impl<T: MessageType> MessageDecoder<T> {
    pub(crate) fn new(cfg: Cfg<HttpServiceConfig>) -> Self {
        MessageDecoder {
            hdrs: Cell::new(false),
            inner: Cell::new(Some(Box::new(Inner {
                cfg,
                st: State::default(),
                val: None,
                hdr: httparse::Header::default(),
                hdr_st: httparse::State::default(),
                consumed: 0,
                headers: 0,
                line: T::Parser::default(),
                line_st: httparse::State::default(),
            }))),
        }
    }

    pub(super) fn is_reading_hdrs(&self) -> bool {
        self.hdrs.get()
    }
}

impl<T: MessageType> Clone for MessageDecoder<T> {
    fn clone(&self) -> Self {
        let inner = self.inner.take().unwrap();
        let val = MessageDecoder {
            hdrs: Cell::new(false),
            inner: Cell::new(Some(Box::new(Inner {
                st: State::default(),
                val: None,
                consumed: 0,
                headers: 0,
                line: T::Parser::default(),
                line_st: httparse::State::default(),
                hdr: httparse::Header::default(),
                hdr_st: httparse::State::default(),
                cfg: inner.cfg.clone(),
            }))),
        };
        self.inner.set(Some(inner));
        val
    }
}

impl<T: MessageType> MessageDecoder<T> {
    fn decode_headers(src: &mut BytesMut, inner: &mut Inner<T>) -> Poll<Result<(), DecodeError>> {
        loop {
            let result = match inner.hdr.parse_with_state(src, &mut inner.hdr_st)? {
                Status::Complete(result) => result,
                Status::Partial => return Poll::Pending,
            };
            match result {
                HeaderParsed::Header(len) => {
                    // repeated header names count separately
                    if inner.headers >= inner.cfg.max_headers {
                        return Poll::Ready(Err(DecodeError::MaxHeaders));
                    }
                    inner.headers += 1;
                    let (n, v) = (inner.hdr.name, inner.hdr.value);
                    // the parser validates name characters, but not its length
                    let Ok(name) = HeaderName::from_bytes(&src[n.start..n.end]) else {
                        return Poll::Ready(Err(DecodeError::Header));
                    };

                    // name and value are split off `src` directly, without
                    // splitting the whole line first, `pos` is the number of
                    // bytes of the line already removed from `src`
                    let mut pos = 0;
                    let origin = if inner.cfg.headers_vec {
                        src.advance_to(n.start);
                        pos = n.end;
                        Some(src.split_to(n.end - n.start))
                    } else {
                        None
                    };
                    let value = if v.start == v.end {
                        Bytes::new()
                    } else {
                        src.advance_to(v.start - pos);
                        pos = v.end;
                        src.split_to(v.end - v.start)
                    };
                    src.advance_to(len - pos);

                    // SAFETY: ntex-httparse checks header value for validity
                    let value = unsafe { HeaderValue::from_shared_unchecked(value) };

                    if let Some(origin) = origin {
                        // SAFETY: ntex-httparse checks header name validity
                        let origin = unsafe { ByteString::from_bytes_unchecked(origin) };
                        inner.val.as_mut().unwrap().set_headers_item(HeaderItem {
                            origin,
                            name: name.clone(),
                            value: value.clone(),
                        });
                    }

                    inner.hdr_st = httparse::State::default();
                    inner
                        .val
                        .as_mut()
                        .unwrap()
                        .set_header(&mut inner.st, name, value)?;
                }
                HeaderParsed::Eof(len) => {
                    src.advance_to(len);
                    inner.hdr_st = httparse::State::default();
                    break;
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl<T: MessageType> Decoder for MessageDecoder<T> {
    type Item = (T, PayloadType);
    type Error = DecodeError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut inner = self.inner.take().unwrap();
        let result = self.decode_message(src, &mut inner);
        if result.is_err() {
            // start the next message from a clean state
            inner.val = None;
            inner.st = State::default();
            inner.hdr_st = httparse::State::default();
            inner.consumed = 0;
            inner.headers = 0;
            inner.line_st = httparse::State::default();
            self.hdrs.set(false);
        }
        self.inner.set(Some(inner));
        result
    }
}

impl<T: MessageType> MessageDecoder<T> {
    fn decode_message(
        &self,
        src: &mut BytesMut,
        inner: &mut Inner<T>,
    ) -> Result<Option<(T, PayloadType)>, DecodeError> {
        if !src.is_empty() {
            self.hdrs.set(true);
        }

        // leading empty lines are not part of the start line and its size
        // limit, positions in `line_st` stay valid only while nothing is
        // removed from `src`
        if inner.val.is_none() && inner.line_st == httparse::State::default() {
            let skip = empty_lines(src);
            if skip > 0 {
                src.advance_to(skip);
                inner.consumed += skip;
            }
        }
        let len = src.len();
        let max_line = inner.cfg.max_start_line_size;
        if inner.val.is_none() {
            // the parser resumes from `line_st`, so data of an incomplete
            // start line is not scanned again on the next read
            match T::decode(src, &mut inner.line, &mut inner.line_st)? {
                Poll::Ready(_) if len - src.len() > max_line => {
                    return Err(DecodeError::StartLineTooLong(len - src.len()));
                }
                Poll::Ready(val) => {
                    inner.line_st = httparse::State::default();
                    inner.st.version = val.msg_version();
                    inner.st.validate_host = T::REQUEST && inner.cfg.validate_host;
                    inner.val = Some(val);
                }
                Poll::Pending => {}
            }
        }
        if inner.val.is_none() && len > max_line {
            return Err(DecodeError::StartLineTooLong(len));
        }

        let (result, buf_size) = if inner.val.is_some() {
            match MessageDecoder::<T>::decode_headers(src, inner) {
                Poll::Ready(Ok(())) => {
                    let mut val = inner.val.take().unwrap();
                    if T::REQUEST {
                        inner.st.check_upgrade();
                    }
                    let pl_len = inner.st.payload_length();
                    let pl = val.set_payload_length(&mut inner.st, pl_len)?;
                    let consumed = inner.consumed + len - src.len();
                    inner.st = State::default();
                    inner.consumed = 0;
                    inner.headers = 0;
                    self.hdrs.set(false);
                    (Ok(Some((val, pl))), consumed)
                }
                Poll::Pending => {
                    let buf_size = inner.consumed + len;
                    inner.consumed = buf_size - src.len();
                    (Ok(None), buf_size)
                }
                Poll::Ready(Err(e)) => (Err(e), 0),
            }
        } else {
            (Ok(None), inner.consumed + len)
        };

        if buf_size > inner.cfg.max_buf_size {
            log::trace!("MAX_BUFFER_SIZE of data reached, closing");
            return Err(DecodeError::TooLarge(buf_size));
        }
        result
    }
}

impl<T: MessageType> fmt::Debug for MessageDecoder<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageDecoder").finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PayloadLength {
    Payload(PayloadType),
    Upgrade,
    None,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: PayloadLength = PayloadLength::Payload(PayloadType::Payload(PayloadDecoder {
    kind: Cell::new(Kind::Length(0)),
}));

impl PayloadLength {
    /// Returns true if variant is `None`.
    fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    #[allow(clippy::borrow_interior_mutable_const)]
    /// Returns true if variant is represents zero-length (not none) payload.
    fn is_zero(&self) -> bool {
        self == &ZERO
    }
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
    struct Flags: u16 {
        const HAS_UPGRADE  = 0b0001;
        const EXPECT       = 0b0010;
        const CHUNKED      = 0b0100;
        const SEEN_TE      = 0b1000;
        const CONN_CLOSE   = 0b0001_0000;
        const CONN_KA      = 0b0010_0000;
        const CONN_UPGRADE = 0b0100_0000;
        const WS_UPGRADE   = 0b1000_0000;
        const SEEN_HOST    = 0b0001_0000_0000;
        const TE_OTHER     = 0b0010_0000_0000;
    }
}

#[derive(Default, Debug)]
pub(crate) struct State {
    flags: Flags,
    content_length: Option<u64>,
    version: Version,
    validate_host: bool,
}

impl State {
    fn connection_types(&self) -> impl Iterator<Item = ConnectionType> {
        [
            (Flags::CONN_CLOSE, ConnectionType::Close),
            (Flags::CONN_KA, ConnectionType::KeepAlive),
            (Flags::CONN_UPGRADE, ConnectionType::Upgrade),
        ]
        .into_iter()
        .filter_map(|(flag, ctype)| self.flags.contains(flag).then_some(ctype))
    }

    /// An upgrade requires both `Upgrade` and the `upgrade` connection option,
    /// see [RFC 9110 section 7.8](https://www.rfc-editor.org/rfc/rfc9110#section-7.8).
    fn check_upgrade(&mut self) {
        if !self
            .flags
            .contains(Flags::HAS_UPGRADE | Flags::CONN_UPGRADE)
        {
            self.flags
                .remove(Flags::HAS_UPGRADE | Flags::WS_UPGRADE | Flags::CONN_UPGRADE);
        }
    }

    fn payload_length(&self) -> PayloadLength {
        // https://tools.ietf.org/html/rfc7230#section-3.3.3
        if self.flags.contains(Flags::CHUNKED) {
            // Chunked encoding
            PayloadLength::Payload(PayloadType::Payload(PayloadDecoder::chunked()))
        } else if let Some(len) = self.content_length
            // some clients (dart) send "content-length: 0" with websocket upgrade
            && !(len == 0 && self.flags.contains(Flags::WS_UPGRADE))
        {
            // Content-Length
            PayloadLength::Payload(PayloadType::Payload(PayloadDecoder::length(len)))
        } else if self.flags.contains(Flags::HAS_UPGRADE) {
            PayloadLength::Upgrade
        } else {
            PayloadLength::None
        }
    }
}

pub(crate) trait MessageType: fmt::Debug + Sized {
    /// `true` for request messages.
    const REQUEST: bool;

    /// Resumable start line parser.
    type Parser: Default;

    fn msg_version(&self) -> Version;

    /// `Expect` and `Upgrade` must be ignored in HTTP/1.0 requests,
    /// see [RFC 9110 section 10.1.1](https://www.rfc-editor.org/rfc/rfc9110#section-10.1.1)
    /// and [section 7.8](https://www.rfc-editor.org/rfc/rfc9110#section-7.8).
    fn ignore_http11_features(st: &State) -> bool {
        Self::REQUEST && st.version < Version::HTTP_11
    }

    fn headers_mut(&mut self) -> &mut HeaderMap;

    /// Decodes the start line, resuming from `st` saved by a previous
    /// `Pending` result for the same buffer.
    fn decode(
        src: &mut BytesMut,
        parser: &mut Self::Parser,
        st: &mut httparse::State,
    ) -> Result<Poll<Self>, DecodeError>;

    fn set_payload_length(
        &mut self,
        st: &mut State,
        length: PayloadLength,
    ) -> Result<PayloadType, DecodeError>;

    fn set_headers_item(&mut self, item: HeaderItem);

    fn set_header(
        &mut self,
        st: &mut State,
        name: HeaderName,
        value: HeaderValue,
    ) -> Result<(), DecodeError> {
        match name {
            header::CONTENT_LENGTH
                if st.content_length.is_some()
                    || st.flags.intersects(Flags::CHUNKED | Flags::TE_OTHER) =>
            {
                log::trace!("multiple Content-Length or Transfer-Encoding with Content-Length");
                return Err(DecodeError::Header);
            }
            header::CONTENT_LENGTH => match value.to_str() {
                Ok(s) if s.trim_start().starts_with('+') => {
                    log::trace!("illegal Content-Length: {s:?}");
                    return Err(DecodeError::Header);
                }
                Ok(s) => {
                    if let Ok(len) = atoi_simd::parse::<u64, true, true>(s.as_bytes()) {
                        // accept 0 lengths here and remove them in `decode` after all
                        // headers have been processed to prevent request smuggling issues
                        st.content_length = Some(len);
                    } else {
                        log::trace!("illegal Content-Length: {s:?}");
                        return Err(DecodeError::Header);
                    }
                }
                Err(_) => {
                    log::trace!("illegal Content-Length: {value:?}");
                    return Err(DecodeError::Header);
                }
            },
            // transfer-encoding
            header::TRANSFER_ENCODING if st.flags.contains(Flags::SEEN_TE) => {
                log::trace!("Transfer-Encoding header usage is not allowed");
                return Err(DecodeError::Header);
            }
            header::TRANSFER_ENCODING if st.version == Version::HTTP_11 => {
                st.flags.insert(Flags::SEEN_TE);
                let Some((chunked, other)) = transfer_codings(value.as_bytes()) else {
                    log::trace!("illegal Transfer-Encoding: {value:?}");
                    return Err(DecodeError::Header);
                };
                if st.content_length.is_some() && (chunked || other) {
                    log::trace!("Transfer-Encoding with Content-Length not allowed");
                    return Err(DecodeError::Header);
                }
                if chunked {
                    st.flags.insert(Flags::CHUNKED);
                } else if other {
                    // a response without final chunked coding is delimited by
                    // connection close, see https://www.rfc-editor.org/rfc/rfc9112#section-6.3
                    st.flags.insert(Flags::TE_OTHER);
                }
                if Self::REQUEST {
                    if !chunked {
                        log::trace!("request without final chunked coding: {value:?}");
                        return Err(DecodeError::Header);
                    }
                    if other {
                        log::trace!("unsupported transfer coding: {value:?}");
                        return Err(DecodeError::UnsupportedTransferCoding);
                    }
                }
            }
            header::TRANSFER_ENCODING if st.version == Version::HTTP_10 => {
                return Err(DecodeError::InvalidInput(
                    "Transfer-Encoding is not supported by HTTP/1.0",
                ));
            }
            // connection keep-alive state
            header::CONNECTION => {
                let mut flags = connection_flags(value.as_bytes());
                if Self::ignore_http11_features(st) {
                    flags.remove(Flags::CONN_UPGRADE);
                }
                st.flags.insert(flags);
            }
            // https://www.rfc-editor.org/rfc/rfc9112#section-3.2
            header::HOST if st.validate_host => {
                if st.flags.contains(Flags::SEEN_HOST) {
                    log::trace!("multiple Host headers not allowed");
                    return Err(DecodeError::Header);
                }
                if !is_valid_host(value.as_bytes()) {
                    log::trace!("illegal Host: {value:?}");
                    return Err(DecodeError::Header);
                }
                st.flags.insert(Flags::SEEN_HOST);
            }
            header::UPGRADE | header::EXPECT if Self::ignore_http11_features(st) => (),
            header::UPGRADE => {
                st.flags.insert(Flags::HAS_UPGRADE);
                if value
                    .as_bytes()
                    .trim_ascii()
                    .eq_ignore_ascii_case(b"websocket")
                {
                    st.flags.insert(Flags::WS_UPGRADE);
                }
            }
            // a list of case-insensitive expectations, only `100-continue`
            // is defined, see RFC 9110 section 10.1.1
            header::EXPECT
                if value
                    .as_bytes()
                    .split(|&b| b == b',')
                    .any(|e| e.trim_ascii().eq_ignore_ascii_case(b"100-continue")) =>
            {
                st.flags.insert(Flags::EXPECT);
            }
            _ => (),
        }

        self.headers_mut().append(name, value);
        Ok(())
    }
}

impl MessageType for Request {
    const REQUEST: bool = true;

    type Parser = httparse::Request;

    fn msg_version(&self) -> Version {
        self.version()
    }

    fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head_mut().headers
    }

    fn decode(
        src: &mut BytesMut,
        req: &mut httparse::Request,
        st: &mut httparse::State,
    ) -> Result<Poll<Self>, DecodeError> {
        match req.parse_with_state(src, st)? {
            Status::Complete(pos) => {
                let method = Method::from_bytes(&src[req.method.start..req.method.end])
                    .map_err(|_| DecodeError::Method)?;
                let target = &src[req.path.start..req.path.end];
                // asterisk-form is only used for a server-wide `OPTIONS` request,
                // see RFC 9112 section 3.2.4
                if target == b"*" && method != Method::OPTIONS {
                    return Err(DecodeError::Uri);
                }
                let uri = Uri::try_from(target)?;
                // authority-form is used only, and always, for `CONNECT`, see
                // RFC 9112 section 3.2.3
                let authority_form = uri.scheme().is_none() && uri.authority().is_some();
                if authority_form != (method == Method::CONNECT) {
                    return Err(DecodeError::Uri);
                }
                let version = if req.version == 1 {
                    Version::HTTP_11
                } else {
                    Version::HTTP_10
                };
                src.advance_to(pos);

                let mut msg = Request::new();
                let head = msg.head_mut();
                head.uri = uri;
                head.method = method;
                head.version = version;
                Ok(Poll::Ready(msg))
            }
            Status::Partial => Ok(Poll::Pending),
        }
    }

    fn set_headers_item(&mut self, item: HeaderItem) {
        self.head_mut().headers_vec.push(item);
    }

    fn set_payload_length(
        &mut self,
        st: &mut State,
        mut length: PayloadLength,
    ) -> Result<PayloadType, DecodeError> {
        // disallow HTTP/1.0 POST requests that do not contain a Content-Length headers
        // see https://datatracker.ietf.org/doc/html/rfc1945#section-7.2.2
        if self.version() == Version::HTTP_10 && self.method() == Method::POST && length.is_none() {
            log::trace!("no Content-Length specified for HTTP/1.0 POST request");
            return Err(DecodeError::Header);
        }
        if st.validate_host
            && self.version() >= Version::HTTP_11
            && !st.flags.contains(Flags::SEEN_HOST)
        {
            log::trace!("no Host header specified for HTTP/1.1 request");
            return Err(DecodeError::Header);
        }

        for ctype in st.connection_types() {
            self.head_mut().set_connection_type(ctype);
        }
        if st.flags.contains(Flags::EXPECT) {
            self.head_mut().set_expect();
        }

        // Remove CL value if 0 now that all headers and HTTP/1.0 special cases are processed.
        // Protects against some request smuggling attacks.
        // See https://github.com/actix/actix-web/issues/2767.
        if length.is_zero() {
            length = PayloadLength::None;
        }

        // payload decoder
        let decoder = match length {
            PayloadLength::Payload(pl) => pl,
            PayloadLength::Upgrade => {
                // upgrade(websocket)
                self.head_mut().set_upgrade();
                PayloadType::Stream(PayloadDecoder::eof())
            }
            PayloadLength::None => {
                if self.method() == Method::CONNECT {
                    self.head_mut().set_upgrade();
                    PayloadType::Stream(PayloadDecoder::eof())
                } else {
                    PayloadType::None
                }
            }
        };

        Ok(decoder)
    }
}

impl MessageType for ResponseHead {
    const REQUEST: bool = false;

    type Parser = httparse::Response;

    fn msg_version(&self) -> Version {
        self.version
    }

    fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    fn decode(
        src: &mut BytesMut,
        res: &mut httparse::Response,
        st: &mut httparse::State,
    ) -> Result<Poll<Self>, DecodeError> {
        match res.parse_with_state(src, st)? {
            Status::Complete(pos) => {
                let version = if res.version == 1 {
                    Version::HTTP_11
                } else {
                    Version::HTTP_10
                };
                let status = StatusCode::from_u16(res.code).map_err(|_| DecodeError::Status)?;

                src.advance_to(pos);
                Ok(Poll::Ready(ResponseHead::new(status, version)))
            }
            Status::Partial => Ok(Poll::Pending),
        }
    }

    fn set_headers_item(&mut self, item: HeaderItem) {
        self.headers_vec.push(item);
    }

    fn set_payload_length(
        &mut self,
        st: &mut State,
        length: PayloadLength,
    ) -> Result<PayloadType, DecodeError> {
        for ctype in st.connection_types() {
            self.set_connection_type(ctype);
        }

        // `1xx` (except `101`), `204` and `304` responses never have a body,
        // `Content-Length` of `304` describes the selected representation
        if is_bodyless(self.status) {
            return Ok(PayloadType::None);
        }

        // message payload
        let decoder = if self.status == StatusCode::SWITCHING_PROTOCOLS
            && (length.is_zero() || !matches!(length, PayloadLength::Payload(_)))
        {
            // switching protocol
            PayloadType::Stream(PayloadDecoder::eof())
        } else if length.is_zero() {
            PayloadType::None
        } else if let PayloadLength::Payload(pl) = length {
            pl
        } else {
            // no declared length, read to eof and close connection
            // see https://www.rfc-editor.org/rfc/rfc9112#section-6.3
            self.set_connection_type(ConnectionType::Close);
            PayloadType::Payload(PayloadDecoder::eof())
        };

        Ok(decoder)
    }
}

/// Collects the connection options listed in a `Connection` header value.
///
/// The value is a comma-separated list of case-insensitive tokens, see
/// [RFC 9110 section 7.6.1](https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1).
/// `Host = uri-host [ ":" port ]`, an empty value is allowed
fn is_valid_host(val: &[u8]) -> bool {
    if val.is_empty() {
        return true;
    }
    if val.contains(&b'@') || Authority::try_from(val).is_err() {
        return false;
    }
    // `Authority` does not validate the port
    let host_end = val.iter().rposition(|&b| b == b']').unwrap_or(0);
    val[host_end..]
        .iter()
        .position(|&b| b == b':')
        .is_none_or(|pos| val[host_end + pos + 1..].iter().all(u8::is_ascii_digit))
}

/// Parses a `Transfer-Encoding` value.
///
/// Returns whether `chunked` is the final transfer coding, and whether other
/// codings are applied, the obsolete `identity` coding is ignored. `None` if
/// the value is malformed or `chunked` is applied more than once, see
/// [RFC 9112 section 6.1](https://www.rfc-editor.org/rfc/rfc9112#section-6.1).
fn transfer_codings(val: &[u8]) -> Option<(bool, bool)> {
    let mut chunked = false;
    let mut seen_chunked = false;
    let mut other = false;
    // empty list elements are allowed, see RFC 9110 section 5.6.1
    for coding in val.split(|&b| b == b',').map(<[u8]>::trim_ascii) {
        if coding.is_empty() {
            continue;
        }
        let name = coding.split(|&b| b == b';').next().unwrap_or_default();
        let name = name.trim_ascii();
        if name.is_empty() || !name.iter().copied().all(is_tchar) {
            return None;
        }
        if name.eq_ignore_ascii_case(b"chunked") {
            // chunked has no parameters
            if seen_chunked || name.len() != coding.len() {
                return None;
            }
            chunked = true;
            seen_chunked = true;
        } else if chunked {
            // chunked is not the final coding
            chunked = false;
            other = true;
        } else if !name.eq_ignore_ascii_case(b"identity") {
            other = true;
        }
    }
    Some((chunked, other))
}

fn connection_flags(val: &[u8]) -> Flags {
    let mut flags = Flags::empty();
    for token in val.split(|&b| b == b',') {
        let token = token.trim_ascii();
        if token.eq_ignore_ascii_case(b"close") {
            flags.insert(Flags::CONN_CLOSE);
        } else if token.eq_ignore_ascii_case(b"keep-alive") {
            flags.insert(Flags::CONN_KA);
        } else if token.eq_ignore_ascii_case(b"upgrade") {
            flags.insert(Flags::CONN_UPGRADE);
        }
    }
    flags
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// A decoded HTTP/1 payload item.
pub enum PayloadItem {
    /// A payload data chunk.
    Chunk(Bytes),
    /// The end of the payload.
    Eof,
}

/// Incremental decoder for an HTTP/1 message body.
///
/// The decoder handles fixed `Content-Length`, chunked transfer coding, and
/// bodies delimited by connection EOF. It implements [`Decoder`] and retains
/// framing state between calls.
///
/// Fixed-length and chunked decoders emit [`PayloadItem::Eof`] when their wire
/// framing completes. An EOF-delimited decoder emits every available byte as a
/// chunk but cannot infer completion from an empty input buffer; the transport
/// owner must treat connection closure as the end of that payload.
///
/// `Ok(None)` means that more bytes or transport EOF are required. A
/// [`DecodeError`] reports malformed payload framing, such as an invalid
/// chunk-size or chunk terminator. Cloning preserves the current payload
/// framing state.
///
/// Chunk extensions and trailer fields are validated and skipped, they are
/// not exposed. A chunked payload is rejected with
/// [`DecodeError::InvalidInput`] if its chunk extensions exceed 16 KiB in
/// total, or if its trailer section, including line terminators, exceeds
/// 4 KiB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadDecoder {
    kind: Cell<Kind>,
}

impl PayloadDecoder {
    pub(super) fn length(x: u64) -> PayloadDecoder {
        PayloadDecoder {
            kind: Cell::new(Kind::Length(x)),
        }
    }

    pub(super) fn chunked() -> PayloadDecoder {
        let limits = ChunkedLimits {
            ext: 0,
            trailers: 0,
            line: 0,
            line_state: SizeLine::Unknown,
        };
        PayloadDecoder {
            kind: Cell::new(Kind::Chunked(ChunkedState::Size, 0, limits)),
        }
    }

    pub(crate) fn eof() -> PayloadDecoder {
        PayloadDecoder {
            kind: Cell::new(Kind::Eof),
        }
    }

    /// Returns `true` if the payload is delimited by connection close.
    pub(crate) fn is_eof(&self) -> bool {
        self.kind.get() == Kind::Eof
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Kind {
    /// A Reader used when a Content-Length header is passed with a positive
    /// integer.
    Length(u64),
    /// A Reader used when Transfer-Encoding is `chunked`.
    ///
    /// Holds the chunked state, the remaining size of the current chunk and
    /// the size limits.
    Chunked(ChunkedState, u64, ChunkedLimits),
    /// A Reader used for responses that don't indicate a length or chunked.
    ///
    /// Note: This should only used for `Response`s. It is illegal for a
    /// `Request` to be made with both `Content-Length` and
    /// `Transfer-Encoding: chunked` missing, as explained from the spec:
    ///
    /// > If a Transfer-Encoding header field is present in a response and
    /// > the chunked transfer coding is not the final encoding, the
    /// > message body length is determined by reading the connection until
    /// > it is closed by the server.  If a Transfer-Encoding header field
    /// > is present in a request and the chunked transfer coding is not
    /// > the final encoding, the message body length cannot be determined
    /// > reliably; the server MUST respond with the 400 (Bad Request)
    /// > status code and then close the connection.
    Eof,
}

/// Maximum number of chunk-size line bytes beyond the size digits, such as
/// chunk extensions, accepted for a chunked payload.
const MAX_CHUNK_EXTENSIONS: u32 = 16 * 1024;

/// Maximum size of the trailer section, including line terminators, accepted
/// for a chunked payload.
const MAX_CHUNK_TRAILERS: u32 = 4 * 1024;

/// Chunks smaller than this are merged with the following chunks.
const SMALL_CHUNK: usize = 1024;

/// Maximum size of merged chunks.
const MAX_MERGED_CHUNKS: usize = 16 * 1024;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct ChunkedLimits {
    /// chunk-size line bytes beyond the size digits received so far
    ext: u32,
    /// trailer section bytes received so far
    trailers: u32,
    /// bytes of a partially received chunk-size line that are validated
    line: u32,
    /// parser state at the end of the validated bytes
    line_state: SizeLine,
}

/// Parser state of a partially received chunk-size line.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum SizeLine {
    /// the line must be parsed from the start
    Unknown,
    /// whitespace after the chunk size
    Lws,
    /// chunk extensions
    Ext,
}

impl SizeLine {
    /// Returns `true` if bytes do not change the parser state.
    fn is_neutral(self, bytes: &[u8]) -> bool {
        match self {
            SizeLine::Unknown => false,
            SizeLine::Lws => bytes.iter().all(|&b| b == b' ' || b == b'\t'),
            // any octet except control characters other than HTAB, `\r` ends the line
            SizeLine::Ext => bytes
                .iter()
                .all(|&b| b == b'\t' || (b >= 0x20 && b != 0x7f)),
        }
    }
}

impl ChunkedLimits {
    fn add_trailers(&mut self, len: usize) -> Result<(), DecodeError> {
        self.trailers = self.trailers.saturating_add(len as u32);
        if self.trailers > MAX_CHUNK_TRAILERS {
            Err(DecodeError::InvalidInput("Chunked trailers are too large"))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum ChunkedState {
    Size,
    Body,
    BodyCr,
    BodyLf,
    EndCr,
    EndLf,
    Trailer,
    TrailerLf,
    End,
}

impl Decoder for PayloadDecoder {
    type Item = PayloadItem;
    type Error = DecodeError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut kind = self.kind.get();

        match kind {
            Kind::Length(ref mut remaining) => {
                if *remaining == 0 {
                    Ok(Some(PayloadItem::Eof))
                } else {
                    if src.is_empty() {
                        return Ok(None);
                    }
                    let len = src.len() as u64;
                    let buf;
                    if *remaining > len {
                        buf = src.take();
                        *remaining -= len;
                    } else {
                        buf = src.split_to(*remaining as usize);
                        *remaining = 0;
                    }
                    self.kind.set(kind);
                    log::trace!("Length read: {}", buf.len());
                    Ok(Some(PayloadItem::Chunk(buf)))
                }
            }
            Kind::Chunked(ref mut state, ref mut size, ref mut limits) => {
                // small chunks are merged into one item, the payload of tiny
                // chunks would be buffered as many items
                let mut data: Option<Bytes> = None;
                let mut merged: Option<BytesMut> = None;
                let result = loop {
                    // a large chunk is not copied into merged chunks
                    if *state == ChunkedState::Body && *size >= SMALL_CHUNK as u64 && data.is_some()
                    {
                        break Ok(None);
                    }

                    let mut buf = None;
                    // advances the chunked state
                    *state = match state.step(src, size, limits, &mut buf) {
                        Poll::Pending => break Ok(None),
                        Poll::Ready(Ok(state)) => state,
                        Poll::Ready(Err(e)) => break Err(e),
                    };

                    if *state == ChunkedState::End {
                        log::trace!("End of chunked stream");
                        break Ok(Some(PayloadItem::Eof));
                    }

                    if let Some(buf) = buf {
                        let len = if let Some(first) = &data {
                            let m = merged.get_or_insert_with(|| {
                                let cap = first.len() + buf.len() + src.len();
                                let mut m = BytesMut::with_capacity(cap.min(MAX_MERGED_CHUNKS));
                                m.extend_from_slice(first);
                                m
                            });
                            m.extend_from_slice(&buf);
                            m.len()
                        } else {
                            let len = buf.len();
                            data = Some(buf);
                            len
                        };
                        if len >= SMALL_CHUNK && (merged.is_none() || len >= MAX_MERGED_CHUNKS) {
                            break Ok(None);
                        }
                    }
                    if src.is_empty() {
                        break Ok(None);
                    }
                };
                self.kind.set(kind);

                // the end of the payload is reported on the next call
                match result {
                    Ok(_) if data.is_some() => {
                        let data = merged.map_or_else(|| data.unwrap(), BytesMut::freeze);
                        Ok(Some(PayloadItem::Chunk(data)))
                    }
                    result => result,
                }
            }
            Kind::Eof => {
                if src.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(PayloadItem::Chunk(src.take())))
                }
            }
        }
    }
}

macro_rules! byte (
    ($rdr:ident) => ({
        if $rdr.len() > 0 {
            let b = $rdr[0];
            $rdr.advance_to(1);
            b
        } else {
            return Poll::Pending
        }
    })
);

impl ChunkedState {
    fn step(
        self,
        body: &mut BytesMut,
        size: &mut u64,
        limits: &mut ChunkedLimits,
        buf: &mut Option<Bytes>,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        match self {
            ChunkedState::Size => ChunkedState::read_size(body, size, limits),
            ChunkedState::Body => ChunkedState::read_body(body, size, buf),
            ChunkedState::BodyCr => ChunkedState::read_body_cr(body),
            ChunkedState::BodyLf => ChunkedState::read_body_lf(body),
            ChunkedState::EndCr => ChunkedState::read_end_cr(body, limits),
            ChunkedState::EndLf => ChunkedState::read_end_lf(body),
            ChunkedState::Trailer => ChunkedState::read_trailer(body, limits),
            ChunkedState::TrailerLf => ChunkedState::read_trailer_lf(body, limits),
            ChunkedState::End => Poll::Ready(Ok(ChunkedState::End)),
        }
    }

    /// Reads a chunk-size line.
    ///
    /// Bytes beyond the size digits, chunk extensions and whitespace, are
    /// ignored but count against [`MAX_CHUNK_EXTENSIONS`] for the whole
    /// payload, which also bounds a partially received line.
    fn read_size(
        rdr: &mut BytesMut,
        size: &mut u64,
        limits: &mut ChunkedLimits,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        // at most 16 size digits and CRLF
        let max = (MAX_CHUNK_EXTENSIONS - limits.ext) as usize + 18;

        // bytes of a partial line are validated once, new bytes that do
        // not change the parser state do not need to parse the line again
        let line = limits.line as usize;
        if line != 0 && line <= rdr.len() && limits.line_state.is_neutral(&rdr[line..]) {
            return if rdr.len() > max {
                Poll::Ready(Err(DecodeError::InvalidInput(
                    "Chunk extensions are too large",
                )))
            } else {
                limits.line = rdr.len() as u32;
                Poll::Pending
            };
        }

        match httparse::parse_chunk_size(rdr) {
            Ok(httparse::Status::Complete((pos, sz))) => {
                limits.line = 0;
                limits.line_state = SizeLine::Unknown;

                let digits = rdr.iter().take_while(|b| b.is_ascii_hexdigit()).count();
                // the line ends with CRLF
                limits.ext = limits.ext.saturating_add((pos - digits - 2) as u32);
                if limits.ext > MAX_CHUNK_EXTENSIONS {
                    return Poll::Ready(Err(DecodeError::InvalidInput(
                        "Chunk extensions are too large",
                    )));
                }
                rdr.advance_to(pos);
                *size = sz;
                if sz > 0 {
                    Poll::Ready(Ok(ChunkedState::Body))
                } else {
                    Poll::Ready(Ok(ChunkedState::EndCr))
                }
            }
            Ok(httparse::Status::Partial) => {
                if rdr.len() > max {
                    return Poll::Ready(Err(DecodeError::InvalidInput(
                        "Chunk extensions are too large",
                    )));
                }
                limits.line = rdr.len() as u32;
                limits.line_state = if rdr.last() == Some(&b'\r') {
                    // `\n` must follow
                    SizeLine::Unknown
                } else if rdr.contains(&b';') {
                    SizeLine::Ext
                } else if rdr.iter().any(|&b| b == b' ' || b == b'\t') {
                    SizeLine::Lws
                } else {
                    SizeLine::Unknown
                };
                Poll::Pending
            }
            Err(_) => Poll::Ready(Err(DecodeError::InvalidInput(
                "Invalid chunk size line: Invalid Size",
            ))),
        }
    }

    fn read_body(
        rdr: &mut BytesMut,
        rem: &mut u64,
        buf: &mut Option<Bytes>,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        log::trace!("Chunked read, remaining={rem:?}");

        let len = rdr.len() as u64;
        if len == 0 {
            Poll::Ready(Ok(ChunkedState::Body))
        } else {
            let slice;
            if *rem > len {
                slice = rdr.take();
                *rem -= len;
            } else {
                slice = rdr.split_to(*rem as usize);
                *rem = 0;
            }
            *buf = Some(slice);
            if *rem > 0 {
                Poll::Ready(Ok(ChunkedState::Body))
            } else {
                Poll::Ready(Ok(ChunkedState::BodyCr))
            }
        }
    }

    fn read_body_cr(rdr: &mut BytesMut) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\r' => Poll::Ready(Ok(ChunkedState::BodyLf)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput("Invalid chunk body CR"))),
        }
    }

    fn read_body_lf(rdr: &mut BytesMut) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\n' => Poll::Ready(Ok(ChunkedState::Size)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput("Invalid chunk body LF"))),
        }
    }

    fn read_end_cr(
        rdr: &mut BytesMut,
        limits: &mut ChunkedLimits,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\r' => Poll::Ready(Ok(ChunkedState::EndLf)),
            // trailer field, must start with a field name character
            b if is_tchar(b) => Poll::Ready(limits.add_trailers(1).map(|()| ChunkedState::Trailer)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput("Invalid chunk end CR"))),
        }
    }

    /// Skips a trailer field line, trailer fields are not exposed.
    ///
    /// The trailer section counts against [`MAX_CHUNK_TRAILERS`].
    fn read_trailer(
        rdr: &mut BytesMut,
        limits: &mut ChunkedLimits,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        for (idx, b) in rdr.iter().enumerate() {
            match *b {
                b'\r' => {
                    rdr.advance_to(idx + 1);
                    return Poll::Ready(
                        limits
                            .add_trailers(idx + 1)
                            .map(|()| ChunkedState::TrailerLf),
                    );
                }
                b'\t' | b' '..=b'~' | 0x80..=0xff => (),
                _ => {
                    return Poll::Ready(Err(DecodeError::InvalidInput(
                        "Invalid chunked trailer field",
                    )));
                }
            }
        }
        let len = rdr.len();
        rdr.clear();
        if let Err(err) = limits.add_trailers(len) {
            Poll::Ready(Err(err))
        } else {
            Poll::Pending
        }
    }

    fn read_trailer_lf(
        rdr: &mut BytesMut,
        limits: &mut ChunkedLimits,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\n' => Poll::Ready(limits.add_trailers(1).map(|()| ChunkedState::EndCr)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput(
                "Invalid chunked trailer field LF",
            ))),
        }
    }

    fn read_end_lf(rdr: &mut BytesMut) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\n' => Poll::Ready(Ok(ChunkedState::End)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput("Invalid chunk end LF"))),
        }
    }
}

/// Returns the length of complete empty lines at the start of `buf`.
fn empty_lines(buf: &[u8]) -> usize {
    let mut pos = 0;
    loop {
        match &buf[pos..] {
            [b'\n', ..] => pos += 1,
            [b'\r', b'\n', ..] => pos += 2,
            _ => return pos,
        }
    }
}

/// Checks for a `tchar`, see [RFC 9110 section 5.6.2](https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2).
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{HttpMessage, header, header::SET_COOKIE};
    use crate::service::cfg::SharedCfg;

    impl PayloadType {
        fn unwrap(self) -> PayloadDecoder {
            if let PayloadType::Payload(pl) = self {
                pl
            } else {
                panic!()
            }
        }

        fn is_unhandled(&self) -> bool {
            matches!(self, PayloadType::Stream(_))
        }
    }

    impl PayloadItem {
        fn chunk(self) -> Bytes {
            match self {
                PayloadItem::Chunk(chunk) => chunk,
                PayloadItem::Eof => panic!("error"),
            }
        }
        fn eof(&self) -> bool {
            matches!(*self, PayloadItem::Eof)
        }
    }

    macro_rules! parse_ready {
        ($e:expr) => {{
            match MessageDecoder::<Request>::default().decode($e) {
                Ok(Some((msg, _))) => msg,
                Ok(_) => unreachable!("Eof during parsing http request"),
                Err(err) => unreachable!("Error during parsing http request: {:?}", err),
            }
        }};
    }

    macro_rules! expect_parse_err {
        ($e:expr) => {{
            match MessageDecoder::<Request>::default().decode($e) {
                Err(_) => (),
                _ => unreachable!("Error expected"),
            }
        }};
    }

    #[test]
    /// Asterisk-form is only valid for `OPTIONS`, RFC 9112 section 3.2.4.
    fn test_asterisk_form_only_for_options() {
        let mut buf = BytesMut::from("OPTIONS * HTTP/1.1\r\nhost: a\r\n\r\n");
        let req = parse_ready!(&mut buf);
        assert_eq!(req.path(), "*");

        for method in ["GET", "POST", "HEAD", "CONNECT"] {
            let mut buf =
                BytesMut::from(format!("{method} * HTTP/1.1\r\nhost: a\r\n\r\n").as_str());
            match MessageDecoder::<Request>::default().decode(&mut buf) {
                Err(DecodeError::Uri) => (),
                res => panic!("{method}: {res:?}"),
            }
        }
    }

    #[test]
    fn test_authority_form_only_for_connect() {
        let mut buf = BytesMut::from("CONNECT example.com:443 HTTP/1.1\r\nhost: a\r\n\r\n");
        let req = parse_ready!(&mut buf);
        assert_eq!(req.uri().authority().unwrap(), "example.com:443");

        let mut buf = BytesMut::from("GET http://example.com/ HTTP/1.1\r\nhost: a\r\n\r\n");
        let req = parse_ready!(&mut buf);
        assert_eq!(req.path(), "/");

        for method in ["GET", "POST", "HEAD", "OPTIONS"] {
            let mut buf = BytesMut::from(
                format!("{method} example.com:443 HTTP/1.1\r\nhost: a\r\n\r\n").as_str(),
            );
            match MessageDecoder::<Request>::default().decode(&mut buf) {
                Err(DecodeError::Uri) => (),
                res => panic!("{method}: {res:?}"),
            }
        }

        for target in ["/", "/test", "http://example.com:443/"] {
            let mut buf =
                BytesMut::from(format!("CONNECT {target} HTTP/1.1\r\nhost: a\r\n\r\n").as_str());
            match MessageDecoder::<Request>::default().decode(&mut buf) {
                Err(DecodeError::Uri) => (),
                res => panic!("{target}: {res:?}"),
            }
        }
    }

    #[test]
    fn test_too_long_header_name() {
        let mut buf = BytesMut::from("GET / HTTP/1.1\r\n");
        let reader = MessageDecoder::<Request>::default();
        assert!(reader.decode(&mut buf).unwrap().is_none());

        // the partial name stays within the buffer limit
        buf.extend_from_slice("a".repeat(64 * 1024 - 16).as_bytes());
        assert!(reader.decode(&mut buf).unwrap().is_none());

        buf.extend_from_slice(b"aaaaaaaaaaaaaaaaaaaa: v\r\n\r\n");
        assert!(matches!(reader.decode(&mut buf), Err(DecodeError::Header)));
    }

    #[test]
    fn test_partial_start_line_is_resumed() {
        let reader = MessageDecoder::<Request>::default();
        let mut buf = BytesMut::from("GET /");
        assert!(reader.decode(&mut buf).unwrap().is_none());

        // an invalid byte is rejected without waiting for the line end
        buf.extend_from_slice(b"\x01");
        assert!(reader.decode(&mut buf).is_err());

        // the decoder starts over after an error
        let mut buf = BytesMut::from("GET /a HTTP/1.1\r\nhost: a\r\n\r\n");
        assert_eq!(reader.decode(&mut buf).unwrap().unwrap().0.path(), "/a");

        // byte by byte
        let reader = MessageDecoder::<Request>::default();
        let mut buf = BytesMut::new();
        for b in b"\r\nGET  /test/path HTTP/1.1\r\nhost: a\r\n\r" {
            buf.extend_from_slice(&[*b]);
            assert!(reader.decode(&mut buf).unwrap().is_none());
        }
        buf.extend_from_slice(b"\n");
        let req = reader.decode(&mut buf).unwrap().unwrap().0;
        assert_eq!(req.path(), "/test/path");
        assert_eq!(req.headers().get("host").unwrap(), "a");

        let reader = MessageDecoder::<ResponseHead>::default();
        let mut buf = BytesMut::new();
        for b in b"HTTP/1.1 404 Not Found\r\n\r" {
            buf.extend_from_slice(&[*b]);
            assert!(reader.decode(&mut buf).unwrap().is_none());
        }
        buf.extend_from_slice(b"\n");
        let res = reader.decode(&mut buf).unwrap().unwrap().0;
        assert_eq!(res.status, StatusCode::NOT_FOUND);

        // partial start lines of different connections on one thread
        let r1 = MessageDecoder::<Request>::default();
        let r2 = MessageDecoder::<Request>::default();
        let mut b1 = BytesMut::from("PUT /one HT");
        let mut b2 = BytesMut::from("DELETE /two HT");
        assert!(r1.decode(&mut b1).unwrap().is_none());
        assert!(r2.decode(&mut b2).unwrap().is_none());
        b1.extend_from_slice(b"TP/1.1\r\nhost: a\r\n\r\n");
        b2.extend_from_slice(b"TP/1.0\r\n\r\n");
        let req = r1.decode(&mut b1).unwrap().unwrap().0;
        assert_eq!((req.method(), req.path()), (&Method::PUT, "/one"));
        let req = r2.decode(&mut b2).unwrap().unwrap().0;
        assert_eq!((req.method(), req.path()), (&Method::DELETE, "/two"));
    }

    #[test]
    fn test_leading_empty_lines() {
        let reader = MessageDecoder::<Request>::default();
        let mut buf = BytesMut::new();
        for _ in 0..10 {
            buf.extend_from_slice(b"\r\n");
            assert!(reader.decode(&mut buf).unwrap().is_none());
            assert!(buf.is_empty());
        }
        buf.extend_from_slice(b"\nGET /test HTTP/1.1\r\nhost: a\r\n\r\n");
        let req = reader.decode(&mut buf).unwrap().unwrap().0;
        assert_eq!(req.path(), "/test");

        // empty lines count towards the buffer limit
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_buf_size(10))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::new();
        let mut res = Ok(None);
        for _ in 0..6 {
            buf.extend_from_slice(b"\r\n");
            res = reader.decode(&mut buf);
            if res.is_err() {
                break;
            }
        }
        assert_eq!(res.err(), Some(DecodeError::TooLarge(12)));
    }

    #[test]
    fn test_max_start_line_size() {
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_start_line_size(32))
            .into();

        // the limit includes the line end
        let line = format!("GET /{} HTTP/1.1\r\n", "a".repeat(16));
        assert_eq!(line.len(), 32);
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(format!("{line}host: a\r\n\r\n").as_str());
        assert!(reader.decode(&mut buf).unwrap().is_some());

        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(17)).as_str());
        assert_eq!(
            reader.decode(&mut buf).err(),
            Some(DecodeError::StartLineTooLong(33))
        );

        // incomplete line
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from("GET /");
        assert!(reader.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice("a".repeat(27).as_bytes());
        assert!(reader.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"a");
        assert_eq!(
            reader.decode(&mut buf).err(),
            Some(DecodeError::StartLineTooLong(33))
        );

        // headers are not limited
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(
            format!("GET / HTTP/1.1\r\nhost: a\r\nx: {}\r\n\r\n", "a".repeat(64)).as_str(),
        );
        assert!(reader.decode(&mut buf).unwrap().is_some());

        // default limit
        let reader = MessageDecoder::<Request>::default();
        let mut buf = BytesMut::from(format!("GET /{}", "a".repeat(16 * 1024)).as_str());
        assert!(matches!(
            reader.decode(&mut buf),
            Err(DecodeError::StartLineTooLong(_))
        ));
    }

    #[test]
    fn test_parse() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");

        let reader = MessageDecoder::<Request>::default();
        match reader.decode(&mut buf) {
            Ok(Some((req, _))) => {
                assert_eq!(req.version(), Version::HTTP_11);
                assert_eq!(*req.method(), Method::GET);
                assert_eq!(req.path(), "/test");
            }
            Ok(_) | Err(_) => unreachable!("Error during parsing http request"),
        }

        let mut buf =
            BytesMut::from("GET /test HTTP/1.1\r\nhost: localhost\r\ncontent-length:512\r\n\r\n");
        let reader = MessageDecoder::<Request>::default();
        let req = reader.decode(&mut buf).unwrap().unwrap().0;
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
    }

    #[test]
    fn test_connection_flags() {
        for s in ["Close", "close,", "close ", " close", "\tclose"] {
            assert_eq!(connection_flags(s.as_bytes()), Flags::CONN_CLOSE);
        }
        for s in ["upgrade", "upGrade", "upgrade,", "upgrade "] {
            assert_eq!(connection_flags(s.as_bytes()), Flags::CONN_UPGRADE);
        }
        for s in ["keep-alive", "keep-Alive", "keep-alive,", "Keep-alive "] {
            assert_eq!(connection_flags(s.as_bytes()), Flags::CONN_KA);
        }
        for s in [
            "keep-aliv",
            "clos",
            "upgrad",
            "closed",
            "close-x",
            "upgrades",
            "x-close",
            "keep-alivex",
            "",
        ] {
            assert_eq!(connection_flags(s.as_bytes()), Flags::empty(), "{s:?}");
        }
        // tokens past the first 5 bytes
        assert_eq!(connection_flags(b"te, trailers, close"), Flags::CONN_CLOSE);
        assert_eq!(
            connection_flags(b"keep-alive, Upgrade"),
            Flags::CONN_KA | Flags::CONN_UPGRADE
        );
    }

    #[test]
    fn test_conn_multiple_tokens() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: te, trailers, close\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);
        assert_eq!(req.head().connection_type(), ConnectionType::Close);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: closed\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);
        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);

        // `close` in an earlier header is not overridden by a later one
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: close\r\n\
             connection: keep-alive\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);
        assert_eq!(req.head().connection_type(), ConnectionType::Close);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             upgrade: websocket\r\n\
             connection: keep-alive, Upgrade\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);
        assert!(req.upgrade());
    }

    #[test]
    fn test_parse_partial() {
        let mut buf = BytesMut::from("PUT /test HTTP/1");

        let reader = MessageDecoder::<Request>::default();
        assert!(reader.decode(&mut buf).unwrap().is_none());

        buf.extend(b".1\r\nhost: localhost\r\n\r\n");
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::PUT);
        assert_eq!(req.path(), "/test");
    }

    #[test]
    fn parse_header_name_origins() {
        // request
        let mut buf = BytesMut::from(
            "GET /test2 HTTP/1.0\r\n\
            Test: 123\r\n\
            Content-Length: 0\r\n\
            \r\n",
        );

        let reader = MessageDecoder::<Request>::new(Cfg::default());
        let (req, _) = reader.decode(&mut buf.clone()).unwrap().unwrap();
        assert_eq!(req.head().headers_vec().len(), 0);

        let cfg: SharedCfg = SharedCfg::new("dbg")
            .add(HttpServiceConfig::default().set_headers_vec(true))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(
            req.head().headers_vec()[0].name,
            HeaderName::try_from("test").unwrap()
        );
        assert_eq!(req.head().headers_vec()[0].origin, "Test");
        assert_eq!(req.head().headers_vec()[1].name, header::CONTENT_LENGTH);
        assert_eq!(req.head().headers_vec()[1].origin, "Content-Length");

        // response
        let mut buf = BytesMut::from(
            "HTTP/1.0 200 Ok\r\n\
            TEST: 123\r\n\
            Content-Length: 0\r\n\
            tesT: 456\r\n\
            \r\n",
        );

        let reader = MessageDecoder::<ResponseHead>::new(Cfg::default());
        let (res, _) = reader.decode(&mut buf.clone()).unwrap().unwrap();
        assert_eq!(res.headers_vec().len(), 0);

        let cfg: SharedCfg = SharedCfg::new("dbg")
            .add(HttpServiceConfig::default().set_headers_vec(true))
            .into();
        let reader = MessageDecoder::<ResponseHead>::new(cfg.get());
        let (res, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(
            res.headers_vec()[0].name,
            HeaderName::try_from("test").unwrap()
        );
        assert_eq!(res.headers_vec()[0].origin, "TEST");
        assert_eq!(res.headers_vec()[0].value, "123");
        assert_eq!(res.headers_vec()[1].name, header::CONTENT_LENGTH);
        assert_eq!(res.headers_vec()[1].origin, "Content-Length");
        assert_eq!(res.headers_vec()[2].origin, "tesT");
        assert_eq!(res.headers_vec()[2].value, "456");
    }

    #[test]
    fn parse_header_values_split_from_buffer() {
        const LONG_NAME: &str = "X-A-Very-Long-Header-Name-Over-Inline";
        const LONG_VALUE: &str = "a value longer than the inline capacity of bytes";
        let text = format!(
            "POST /test HTTP/1.1\r\n\
            Host: a\r\n\
            X-Empty:\r\n\
            X-Ws: \t \r\n\
            {LONG_NAME}: \t{LONG_VALUE} \t\r\n\
            Short: v\r\n\
            Content-Length: 4\r\n\
            \r\n\
            body"
        );

        for headers_vec in [false, true] {
            let cfg: SharedCfg = SharedCfg::new("dbg")
                .add(HttpServiceConfig::default().set_headers_vec(headers_vec))
                .into();
            for chunk in [text.len(), 1] {
                let reader = MessageDecoder::<Request>::new(cfg.get());
                let mut buf = BytesMut::new();
                let mut req = None;
                let mut fed = 0;
                for part in text.as_bytes().chunks(chunk) {
                    buf.extend_from_slice(part);
                    fed += part.len();
                    if let Some((r, _)) = reader.decode(&mut buf).unwrap() {
                        req = Some(r);
                        break;
                    }
                }
                buf.extend_from_slice(&text.as_bytes()[fed..]);
                let req = req.unwrap();
                let ctx = format!("headers_vec {headers_vec} chunk {chunk}");
                assert_eq!(&buf[..], b"body", "{ctx}");

                let h = req.headers();
                assert_eq!(h.get("x-empty").unwrap(), "", "{ctx}");
                assert_eq!(h.get("x-ws").unwrap(), "", "{ctx}");
                assert_eq!(h.get(LONG_NAME).unwrap(), LONG_VALUE, "{ctx}");
                assert_eq!(h.get("short").unwrap(), "v", "{ctx}");
                assert_eq!(h.get(header::CONTENT_LENGTH).unwrap(), "4", "{ctx}");

                let items = req.head().headers_vec();
                if headers_vec {
                    let origins: Vec<_> = items.iter().map(|i| &i.origin[..]).collect();
                    assert_eq!(
                        origins,
                        [
                            "Host",
                            "X-Empty",
                            "X-Ws",
                            LONG_NAME,
                            "Short",
                            "Content-Length"
                        ],
                        "{ctx}"
                    );
                    let values: Vec<_> = items.iter().map(|i| i.value.as_bytes()).collect();
                    let expected: [&[u8]; 6] = [b"a", b"", b"", LONG_VALUE.as_bytes(), b"v", b"4"];
                    assert_eq!(values, expected, "{ctx}");
                } else {
                    assert!(items.is_empty(), "{ctx}");
                }
            }
        }
    }

    #[test]
    fn parse_h10_get() {
        let mut buf = BytesMut::from(
            "GET /test1 HTTP/1.0\r\n\
            \r\n\
            abc",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test1");

        let mut buf = BytesMut::from(
            "GET /test2 HTTP/1.0\r\n\
            Test: 123\r\n\
            Content-Length: 0\r\n\
            \r\n",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test2");

        let mut buf = BytesMut::from(
            "GET /test3?test=1 HTTP/1.0\r\n\
            Content-Length: 3\r\n\
            \r\n
            abc",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test3");
        assert_eq!(req.uri().query(), Some("test=1"));

        // transfer-encoding is not supported for http1.0
        let mut buf =
            BytesMut::from("GET /test3?test=1 HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n");
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn parse_h10_post() {
        let mut buf = BytesMut::from(
            "POST /test1 HTTP/1.0\r\n\
            Content-Length: 3\r\n\
            \r\n\
            abc",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::POST);
        assert_eq!(req.path(), "/test1");

        let mut buf = BytesMut::from(
            "POST /test2 HTTP/1.0\r\n\
            Content-Length: 0\r\n\
            \r\n",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::POST);
        assert_eq!(req.path(), "/test2");

        let mut buf = BytesMut::from(
            "POST /test3 HTTP/1.0\r\n\
            \r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let err = reader.decode(&mut buf).unwrap_err();
        assert!(err.to_string().contains("Header"));
    }

    #[test]
    fn test_parse_body() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\nContent-Length: 4\r\n\r\nbody",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"body"
        );
    }

    #[test]
    fn test_parse_body_crlf() {
        let mut buf = BytesMut::from(
            "\r\nGET /test HTTP/1.1\r\nhost: localhost\r\nContent-Length: 4\r\n\r\nbody",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"body"
        );
    }

    #[test]
    fn test_parse_partial_eof() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\nhost: localhost\r\n");
        let reader = MessageDecoder::<Request>::default();
        assert!(reader.decode(&mut buf).unwrap().is_none());

        buf.extend(b"\r\n");
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
    }

    #[test]
    fn test_headers_split_field() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\nhost: localhost\r\n");

        let reader = MessageDecoder::<Request>::default();
        assert! { reader.decode(&mut buf).unwrap().is_none() }

        buf.extend(b"t");
        assert! { reader.decode(&mut buf).unwrap().is_none() }

        buf.extend(b"es");
        assert! { reader.decode(&mut buf).unwrap().is_none() }

        buf.extend(b"t: value\r\n\r\n");
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test");
        assert_eq!(
            req.headers()
                .get(HeaderName::try_from("test").unwrap())
                .unwrap()
                .as_bytes(),
            b"value"
        );
    }

    #[test]
    fn test_headers_multi_value() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             Set-Cookie: c1=cookie1\r\n\
             Set-Cookie: c2=cookie2\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();

        let val: Vec<_> = req
            .headers()
            .get_all(SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        assert_eq!(val[0], "c1=cookie1");
        assert_eq!(val[1], "c2=cookie2");
    }

    #[test]
    fn test_conn_default_1_0() {
        let mut buf = BytesMut::from("GET /test HTTP/1.0\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::Close);
    }

    #[test]
    fn test_conn_default_1_1() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\nhost: localhost\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
    }

    #[test]
    fn test_conn_close() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: close\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::Close);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: Close\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::Close);
    }

    #[test]
    fn test_conn_close_1_0() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: close\r\n\r\n",
        );

        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::Close);
    }

    #[test]
    fn test_conn_keep_alive_1_0() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: keep-alive\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: Keep-Alive\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
    }

    #[test]
    fn test_conn_keep_alive_1_1() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: keep-alive\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
    }

    #[test]
    fn test_conn_other_1_0() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: other\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::Close);
    }

    #[test]
    fn test_conn_other_1_1() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: other\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
    }

    #[test]
    fn test_conn_upgrade() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             upgrade: websockets\r\n\
             connection: upgrade\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
        assert_eq!(req.head().connection_type(), ConnectionType::Upgrade);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             upgrade: Websockets\r\n\
             connection: Upgrade\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
        assert_eq!(req.head().connection_type(), ConnectionType::Upgrade);
    }

    #[test]
    fn test_upgrade_requires_connection_option() {
        let reader = MessageDecoder::<Request>::default();
        for req in [
            "GET /test HTTP/1.1\r\nhost: a\r\nupgrade: websocket\r\n\r\n",
            "GET /test HTTP/1.1\r\nhost: a\r\nconnection: upgrade\r\n\r\n",
            "GET /test HTTP/1.1\r\nhost: a\r\nconnection: keep-alive\r\n\
             upgrade: websocket\r\ncontent-length: 0\r\n\r\n",
        ] {
            let mut buf =
                BytesMut::from(format!("{req}GET /next HTTP/1.1\r\nhost: a\r\n\r\n").as_str());
            let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
            assert!(!req.upgrade(), "{req:?}");
            assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
            assert_eq!(pl, PayloadType::None);
            // the next request is not consumed as upgraded stream
            let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
            assert_eq!(req.path(), "/next");
        }

        // a body is not treated as an upgraded stream
        let mut buf = BytesMut::from(
            "POST /test HTTP/1.1\r\nhost: a\r\nupgrade: h2c\r\n\
             content-length: 4\r\n\r\nbody",
        );
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        assert!(!req.upgrade());
        assert!(matches!(pl, PayloadType::Payload(_)));
        assert_eq!(req.headers().get(header::UPGRADE).unwrap(), "h2c");

        // both are present
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: a\r\nconnection: keep-alive, Upgrade\r\n\
             upgrade: websocket\r\n\r\n",
        );
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        assert!(req.upgrade());
        assert!(matches!(pl, PayloadType::Stream(_)));
    }

    #[test]
    fn test_expect_100_continue() {
        let reader = MessageDecoder::<Request>::default();
        for (val, expect) in [
            ("100-continue", true),
            ("100-Continue", true),
            (" 100-CONTINUE ", true),
            ("foo, 100-continue", true),
            ("100-foo", false),
            ("100-continuex", false),
            ("100", false),
            ("", false),
        ] {
            let mut buf = BytesMut::from(
                format!(
                    "POST /test HTTP/1.1\r\nhost: a\r\nexpect: {val}\r\ncontent-length: 1\r\n\r\n"
                )
                .as_str(),
            );
            let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
            assert_eq!(req.head().expect(), expect, "{val:?}");
        }
    }

    #[test]
    fn test_http10_ignores_expect_and_upgrade() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.0\r\n\
             connection: keep-alive, upgrade\r\n\
             upgrade: websocket\r\n\
             expect: 100-continue\r\n\r\n\
             GET /next HTTP/1.0\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        assert!(!req.upgrade());
        assert!(!req.head().expect());
        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
        assert_eq!(pl, PayloadType::None);
        // the headers are still available
        assert_eq!(req.headers().get(header::UPGRADE).unwrap(), "websocket");
        assert_eq!(req.headers().get(header::EXPECT).unwrap(), "100-continue");
        // the next request is not consumed as upgraded stream
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.path(), "/next");

        // HTTP/1.1 is not affected
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: upgrade\r\n\
             upgrade: websocket\r\n\
             expect: 100-continue\r\n\r\n",
        );
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        assert!(req.upgrade());
        assert!(req.head().expect());
        assert!(matches!(pl, PayloadType::Stream(_)));
    }

    #[test]
    fn test_host_validation() {
        let reader = MessageDecoder::<Request>::default();
        for req in [
            "GET / HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: a\r\nhost: a\r\n\r\n",
            "GET / HTTP/1.0\r\nhost: a\r\nhost: b\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: user@example.com\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: exa mple.com\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: example.com/path\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: example.com:port\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: [::1]:x\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: a:1:2\r\n\r\n",
        ] {
            let mut buf = BytesMut::from(req);
            assert_eq!(
                reader.decode(&mut buf).err(),
                Some(DecodeError::Header),
                "{req:?}"
            );
        }

        for req in [
            "GET / HTTP/1.0\r\n\r\n",
            "GET / HTTP/1.1\r\nhost:\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: example.com\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: example.com:8080\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: 127.0.0.1:80\r\n\r\n",
            "GET / HTTP/1.1\r\nhost: [::1]:80\r\n\r\n",
            "GET http://example.com/ HTTP/1.1\r\nhost: example.com\r\n\r\n",
        ] {
            let mut buf = BytesMut::from(req);
            assert!(reader.decode(&mut buf).unwrap().is_some(), "{req:?}");
        }

        // validation can be disabled
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_host_validation(false))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(
            "GET / HTTP/1.1\r\n\r\n\
             GET / HTTP/1.1\r\nhost: a\r\nhost: user@b\r\n\r\n",
        );
        assert!(reader.decode(&mut buf).unwrap().is_some());
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.headers().get_all(header::HOST).count(), 2);

        // responses are not affected
        let reader = MessageDecoder::<ResponseHead>::default();
        let mut buf = BytesMut::from("HTTP/1.1 200 OK\r\nhost: a\r\nhost: b\r\n\r\n");
        assert!(reader.decode(&mut buf).unwrap().is_some());
    }

    #[test]
    fn test_conn_upgrade_connect_method() {
        let mut buf = BytesMut::from(
            "CONNECT localhost:443 HTTP/1.1\r\nhost: localhost\r\n\
             content-type: text/plain\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
    }

    #[test]
    fn test_request_chunked() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        if let Ok(val) = req.chunked() {
            assert!(val);
        } else {
            unreachable!("Error");
        }

        // typo in chunked
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             transfer-encoding: chnked\r\n\r\n",
        );
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_headers_content_length_err_1() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             content-length: line\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_headers_content_length_err_2() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             content-length: -1\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_invalid_header() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             test line\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_invalid_name() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             test[]: line\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_bad_status_line() {
        let mut buf = BytesMut::from("getpath \r\n\r\n");
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_upgrade() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             connection: upgrade\r\n\
             upgrade: websocket\r\n\r\n\
             some raw data",
        );
        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.head().connection_type(), ConnectionType::Upgrade);
        assert!(req.upgrade());
        assert!(pl.is_unhandled());
    }

    #[test]
    fn test_http_request_upgrade_content_length() {
        let reader = MessageDecoder::<Request>::default();

        // zero content-length is ignored regardless of header order
        for hdrs in [
            "content-length: 0\r\nupgrade: websocket\r\n",
            "upgrade: websocket\r\ncontent-length: 0\r\n",
        ] {
            let mut buf = BytesMut::from(
                format!(
                    "GET /test HTTP/1.1\r\nhost: localhost\r\nconnection: upgrade\r\n{hdrs}\r\nraw"
                )
                .as_str(),
            );
            let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
            assert!(req.upgrade(), "{hdrs:?}");
            assert!(pl.is_unhandled(), "{hdrs:?}");
        }

        // non-zero content-length delimits the body regardless of header order
        for hdrs in [
            "content-length: 4\r\nupgrade: websocket\r\n",
            "upgrade: websocket\r\ncontent-length: 4\r\n",
        ] {
            let mut buf = BytesMut::from(
                format!(
                    "GET /test HTTP/1.1\r\nhost: localhost\r\nconnection: upgrade\r\n{hdrs}\r\ndata"
                )
                .as_str(),
            );
            let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
            let pl = pl.unwrap();
            assert_eq!(
                pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
                b"data"
            );
            assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
        }

        // duplicate content-length is rejected even after websocket upgrade
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             content-length: 4\r\n\
             upgrade: websocket\r\n\
             content-length: 10\r\n\r\n",
        );
        assert!(reader.decode(&mut buf).is_err());
    }

    #[test]
    fn test_http_request_parser_utf8() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             x-test: тест\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(
            req.headers().get("x-test").unwrap().as_bytes(),
            "тест".as_bytes()
        );
    }

    #[test]
    fn test_http_request_parser_two_slashes() {
        let mut buf = BytesMut::from("GET //path HTTP/1.1\r\nhost: localhost\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert_eq!(req.path(), "//path");
    }

    #[test]
    fn test_http_request_parser_bad_method() {
        let mut buf = BytesMut::from("!12%()+=~$ /get HTTP/1.1\r\nhost: localhost\r\n\r\n");

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_parser_bad_version() {
        let mut buf = BytesMut::from("GET //get HT/11\r\n\r\n");

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_http_request_chunked_payload() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n");
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"dataline"
        );
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_http_request_chunked_payload_and_next_message() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(
            b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n\
              POST /test2 HTTP/1.1\r\nhost: localhost\r\n\
              transfer-encoding: chunked\r\n\r\n"
                .iter(),
        );
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"dataline");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert!(msg.eof());

        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert!(req.chunked().unwrap());
        assert_eq!(*req.method(), Method::POST);
        assert!(req.chunked().unwrap());
    }

    #[test]
    fn test_http_request_chunked_payload_chunks() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(b"4\r\n1111\r\n");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"1111");

        buf.extend(b"4\r\ndata\r");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"data");

        buf.extend(b"\n4");
        assert!(pl.decode(&mut buf).unwrap().is_none());

        buf.extend(b"\r");
        assert!(pl.decode(&mut buf).unwrap().is_none());
        buf.extend(b"\n");
        assert!(pl.decode(&mut buf).unwrap().is_none());

        buf.extend(b"li");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"li");

        buf.extend(b"ne\r\n0\r\n");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"ne");
        assert!(pl.decode(&mut buf).unwrap().is_none());

        buf.extend(b"\r\n");
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_chunked_trailers_limit() {
        let decode = |trailers: &[u8], split: bool| {
            let (pl, mut buf) = chunked_payload();
            buf.extend_from_slice(b"4\r\ndata\r\n0\r\n");
            assert_eq!(pl.decode(&mut buf).unwrap().unwrap().chunk().len(), 4);

            let mut data = trailers.to_vec();
            data.extend_from_slice(b"\r\n");
            let step = if split { 1 } else { data.len() };
            for part in data.chunks(step) {
                buf.extend_from_slice(part);
                match pl.decode(&mut buf) {
                    Ok(None) => (),
                    Ok(Some(item)) => return Ok(item.eof()),
                    Err(err) => return Err(err),
                }
            }
            Ok(false)
        };
        let max = MAX_CHUNK_TRAILERS as usize;

        // a single field
        let field = |len: usize| format!("x: {}\r\n", "v".repeat(len - 5)).into_bytes();
        for split in [false, true] {
            assert_eq!(decode(&field(max), split), Ok(true), "{split}");
            assert!(
                matches!(
                    decode(&field(max + 1), split),
                    Err(DecodeError::InvalidInput(_))
                ),
                "{split}"
            );
            // an endless field line
            assert!(
                matches!(
                    decode(&[&b"x: "[..], &vec![b'v'; max * 2]].concat(), split),
                    Err(DecodeError::InvalidInput(_))
                ),
                "{split}"
            );
        }

        // many small fields
        assert_eq!(decode(&b"x: y\r\n".repeat(max / 6), false), Ok(true));
        assert!(matches!(
            decode(&b"x: y\r\n".repeat(max / 6 + 1), false),
            Err(DecodeError::InvalidInput(_))
        ));
    }

    #[test]
    fn test_parse_chunked_payload_trailers() {
        let mut buf = BytesMut::from(
            "POST /test HTTP/1.1\r\nhost: localhost\r\n\
             transfer-encoding: chunked\r\n\r\n\
             4\r\ndata\r\n0\r\n\
             test: test\r\n\
             x-checksum: \tabc 123\r\n\r\n\
             GET /next HTTP/1.1\r\nhost: localhost\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"data"
        );
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.path(), "/next");
        assert!(buf.is_empty());

        // trailers split across reads
        let mut buf = BytesMut::from(
            "POST /test HTTP/1.1\r\nhost: localhost\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        for part in ["0\r\n", "te", "st: te", "st\r", "\n", "\r"] {
            buf.extend(part.as_bytes());
            assert!(pl.decode(&mut buf).unwrap().is_none(), "{part:?}");
        }
        buf.extend(b"\n");
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());

        // invalid trailers
        for trailer in [
            "test: te\nst\r\n\r\n",
            "test\x00\r\n\r\n",
            " test: v\r\n\r\n",
            "test: v\rx",
        ] {
            let mut buf = BytesMut::from(
                "POST /test HTTP/1.1\r\nhost: localhost\r\n\
                 transfer-encoding: chunked\r\n\r\n0\r\n",
            );
            buf.extend(trailer.as_bytes());
            let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
            assert!(pl.unwrap().decode(&mut buf).is_err(), "{trailer:?}");
        }
    }

    #[test]
    fn test_parse_chunked_payload_chunk_extension() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\nhost: localhost\r\n\
              transfer-encoding: chunked\r\n\r\n",
        );

        let reader = MessageDecoder::<Request>::default();
        let (msg, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert!(msg.chunked().unwrap());

        buf.extend(b"4;test\r\ndata\r\n4\r\nline\r\n0\r\n\r\n"); // test: test\r\n\r\n")
        let chunk = pl.decode(&mut buf).unwrap().unwrap().chunk();
        assert_eq!(chunk, Bytes::from_static(b"dataline"));
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert!(msg.eof());
    }

    fn decode_all(pl: &PayloadDecoder, buf: &mut BytesMut) -> Vec<Bytes> {
        let mut items = Vec::new();
        while let Some(item) = pl.decode(buf).unwrap() {
            match item {
                PayloadItem::Chunk(chunk) => items.push(chunk),
                PayloadItem::Eof => break,
            }
        }
        items
    }

    #[test]
    fn test_small_chunks_are_merged() {
        let (pl, mut buf) = chunked_payload();
        for _ in 0..40_000 {
            buf.extend_from_slice(b"1\r\na\r\n");
        }
        buf.extend_from_slice(b"0\r\n\r\n");
        let items = decode_all(&pl, &mut buf);
        let lens: Vec<_> = items.iter().map(Bytes::len).collect();
        assert_eq!(
            lens,
            [
                MAX_MERGED_CHUNKS,
                MAX_MERGED_CHUNKS,
                40_000 - 2 * MAX_MERGED_CHUNKS
            ]
        );
        assert!(items.iter().all(|c| c.iter().all(|b| *b == b'a')));
        assert!(buf.is_empty());

        // merged chunks are returned before the end of the payload
        let (pl, mut buf) = chunked_payload();
        buf.extend_from_slice(b"2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n");
        assert_eq!(
            pl.decode(&mut buf).unwrap(),
            Some(PayloadItem::Chunk("abcd".into()))
        );
        assert_eq!(pl.decode(&mut buf).unwrap(), Some(PayloadItem::Eof));

        // incomplete chunk
        let (pl, mut buf) = chunked_payload();
        buf.extend_from_slice(b"2\r\nab\r\n5\r\ncd");
        assert_eq!(
            pl.decode(&mut buf).unwrap(),
            Some(PayloadItem::Chunk("abcd".into()))
        );
        assert_eq!(pl.decode(&mut buf).unwrap(), None);
        buf.extend_from_slice(b"efg\r\n0\r\n\r\n");
        assert_eq!(
            pl.decode(&mut buf).unwrap(),
            Some(PayloadItem::Chunk("efg".into()))
        );
        assert_eq!(pl.decode(&mut buf).unwrap(), Some(PayloadItem::Eof));
    }

    #[test]
    fn test_large_chunks_are_not_merged() {
        let large = "x".repeat(SMALL_CHUNK);
        let (pl, mut buf) = chunked_payload();
        buf.extend_from_slice(format!("3\r\nabc\r\n{:X}\r\n{large}\r\n", large.len()).as_bytes());
        buf.extend_from_slice(
            format!("{:X}\r\n{large}\r\n2\r\nxy\r\n0\r\n\r\n", large.len()).as_bytes(),
        );
        let range = buf.as_ptr() as usize..buf.as_ptr() as usize + buf.len();

        let items = decode_all(&pl, &mut buf);
        assert_eq!(
            items,
            [
                Bytes::from("abc"),
                large.clone().into(),
                large.into(),
                "xy".into()
            ]
        );
        // large chunks are not copied
        assert!(range.contains(&(items[1].as_ptr() as usize)));
        assert!(range.contains(&(items[2].as_ptr() as usize)));
    }

    fn chunked_payload() -> (PayloadDecoder, BytesMut) {
        let mut buf = BytesMut::from(
            "POST /test HTTP/1.1\r\nhost: localhost\r\ntransfer-encoding: chunked\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
        (pl.unwrap(), buf)
    }

    #[test]
    fn test_chunk_extension_control_chars() {
        for line in [
            &b"4;a\nX\r\n"[..],
            b"4;a=\"\nX\"\r\n",
            b"4;a\x00\r\n",
            b"4;a\x7f\r\n",
            b"4 \n;a\r\n",
        ] {
            let (pl, mut buf) = chunked_payload();
            buf.extend_from_slice(line);
            buf.extend_from_slice(b"data\r\n0\r\n\r\n");
            assert!(
                matches!(pl.decode(&mut buf), Err(DecodeError::InvalidInput(_))),
                "{line:?}"
            );
        }

        let (pl, mut buf) = chunked_payload();
        buf.extend_from_slice(b"4 ;a=\"b\tc \x80\";d\t\r\ndata\r\n0\r\n\r\n");
        let chunk = pl.decode(&mut buf).unwrap().unwrap().chunk();
        assert_eq!(chunk, Bytes::from_static(b"data"));
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_chunk_extensions_limit() {
        // a size line that never ends is not buffered without limit
        let (pl, mut buf) = chunked_payload();
        buf.extend(b"1;");
        let mut failed = false;
        for _ in 0..64 {
            buf.extend(&[b'a'; 1024]);
            match pl.decode(&mut buf) {
                Ok(None) => (),
                Err(_) => {
                    failed = true;
                    break;
                }
                Ok(Some(item)) => panic!("unexpected item {item:?}"),
            }
        }
        assert!(failed);
        assert!(buf.len() <= MAX_CHUNK_EXTENSIONS as usize + 1024 + 20);

        // extensions are limited for the whole payload
        let (pl, mut buf) = chunked_payload();
        let ext = "a".repeat(1023);
        let mut result = Ok(());
        for _ in 0..32 {
            buf.extend(format!("1;{ext}\r\nx\r\n").as_bytes());
            match pl.decode(&mut buf) {
                Ok(Some(PayloadItem::Chunk(chunk))) => assert_eq!(chunk, "x"),
                Ok(item) => panic!("unexpected item {item:?}"),
                Err(err) => {
                    result = Err(err);
                    break;
                }
            }
        }
        assert!(result.is_err());

        // extensions up to the limit are accepted
        let (pl, mut buf) = chunked_payload();
        let ext = "a".repeat(MAX_CHUNK_EXTENSIONS as usize - 1);
        buf.extend(format!("10;{ext}\r\n0123456789abcdef\r\n0\r\n\r\n").as_bytes());
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk(),
            "0123456789abcdef"
        );
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_chunk_size_line_byte_by_byte() {
        let feed = |line: &[u8]| {
            let (pl, mut buf) = chunked_payload();
            let mut data = Vec::new();
            for (idx, b) in line.iter().enumerate() {
                buf.extend_from_slice(&[*b]);
                match pl.decode(&mut buf) {
                    Ok(None) => (),
                    Ok(Some(item)) => data.extend_from_slice(&item.chunk()),
                    Err(_) => return Err(idx),
                }
            }
            Ok(data)
        };

        // valid lines
        for line in [
            &b"4;a=b;c\r\ndata"[..],
            b"4 \t ;ext\t\x80\r\ndata",
            b"4  \r\ndata",
            b"4\r\ndata",
        ] {
            assert_eq!(feed(line).unwrap(), b"data", "{line:?}");
        }

        // invalid lines fail at the invalid byte
        for (line, pos) in [
            (&b"4;aaaa\x01aaaa\r\n"[..], 6),
            (b"4;aa\x7f", 4),
            (b"4;aa\ra", 5),
            (b"4;aa\na", 4),
            (b"4   5", 4),
            (b"4  x", 3),
            (b"4\rx", 2),
        ] {
            assert_eq!(feed(line).map(|_| ()), Err(pos), "{line:?}");
        }

        // long extension fed byte by byte
        let mut line = b"10;".to_vec();
        line.extend(std::iter::repeat_n(b'a', MAX_CHUNK_EXTENSIONS as usize - 1));
        line.extend_from_slice(b"\r\n0123456789abcdef");
        assert_eq!(feed(&line).unwrap(), b"0123456789abcdef");

        let mut line = b"10;".to_vec();
        line.extend(std::iter::repeat_n(b'a', MAX_CHUNK_EXTENSIONS as usize + 1));
        line.extend_from_slice(b"\r\n");
        assert!(feed(&line).is_err());

        // a line that never ends fails at the limit
        let line = vec![b'a'; MAX_CHUNK_EXTENSIONS as usize];
        let line = [&b"1;"[..], &line, &line].concat();
        assert!(feed(&line).is_err());
    }

    #[test]
    fn test_response_bodyless_status() {
        for (head, rest) in [
            (
                "HTTP/1.1 304 Not Modified\r\ncontent-length: 10\r\n\r\n",
                "",
            ),
            ("HTTP/1.1 204 No Content\r\ncontent-length: 10\r\n\r\n", ""),
            (
                "HTTP/1.1 204 No Content\r\ntransfer-encoding: chunked\r\n\r\n",
                "",
            ),
            (
                "HTTP/1.1 100 Continue\r\ncontent-length: 2\r\n\r\n",
                "HTTP/1.1 200 OK\r\n\r\n",
            ),
            ("HTTP/1.0 304 Not Modified\r\n\r\n", "next"),
        ] {
            let mut buf = BytesMut::from(format!("{head}{rest}").as_str());
            let reader = MessageDecoder::<ResponseHead>::default();
            let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
            assert!(matches!(pl, PayloadType::None), "{head:?}");
            assert_eq!(buf, rest.as_bytes(), "{head:?}");
        }

        let mut buf = BytesMut::from("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
        let reader = MessageDecoder::<ResponseHead>::default();
        let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"ok"
        );
    }

    #[test]
    fn test_response_read_until_eof() {
        for version in ["1.0", "1.1"] {
            let mut buf =
                BytesMut::from(format!("HTTP/{version} 200 OK\r\n\r\ntest data").as_str());
            let reader = MessageDecoder::<ResponseHead>::default();
            let (msg, pl) = reader.decode(&mut buf).unwrap().unwrap();
            assert_eq!(msg.connection_type(), ConnectionType::Close, "{version}");
            let pl = pl.unwrap();
            assert!(pl.is_eof(), "{version}");
            let chunk = pl.decode(&mut buf).unwrap().unwrap();
            assert_eq!(chunk, PayloadItem::Chunk(Bytes::from_static(b"test data")));
        }

        // zero content-length has no payload
        for version in ["1.0", "1.1"] {
            let mut buf = BytesMut::from(
                format!("HTTP/{version} 200 OK\r\ncontent-length: 0\r\n\r\n").as_str(),
            );
            let reader = MessageDecoder::<ResponseHead>::default();
            let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
            assert!(matches!(pl, PayloadType::None), "{version}");
        }

        let mut buf =
            BytesMut::from("HTTP/1.1 101 Switching Protocols\r\ncontent-length: 0\r\n\r\n");
        let reader = MessageDecoder::<ResponseHead>::default();
        let (_, pl) = reader.decode(&mut buf).unwrap().unwrap();
        assert!(matches!(pl, PayloadType::Stream(_)));
    }

    #[test]
    fn test_response_http10_read_until_eof() {
        let mut buf = BytesMut::from("HTTP/1.0 200 Ok\r\n\r\ntest data");

        let reader = MessageDecoder::<ResponseHead>::default();
        let res = reader.decode(&mut buf);
        let (_msg, pl) = res.unwrap().unwrap();
        let pl = pl.unwrap();

        let chunk = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(chunk, PayloadItem::Chunk(Bytes::from_static(b"test data")));
    }

    #[test]
    fn test_multiple_content_length() {
        let mut buf = BytesMut::from(
            "GET / HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 4\r\n\
             Content-Length: 2\r\n\
             \r\n\
             abcd",
        );
        expect_parse_err!(&mut buf);

        let mut buf = BytesMut::from(
            "GET / HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 0\r\n\
             Content-Length: 2\r\n\
             \r\n\
             ab",
        );
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_transfer_encoding_http10() {
        // in HTTP/1.0 transfer encoding is not supported

        let mut buf = BytesMut::from(
            "POST / HTTP/1.0\r\n\
            Host: example.com\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            3\r\n\
            aaa\r\n\
            0\r\n\
            ",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_content_length_and_te_http10() {
        // in HTTP/1.0 transfer encoding is not supported

        let mut buf = BytesMut::from(
            "GET / HTTP/1.0\r\n\
            Host: example.com\r\n\
            Content-Length: 3\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            000",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_content_length_plus() {
        let mut buf = BytesMut::from(
            "GET / HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: +3\r\n\
             \r\n\
             000",
        );
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_unknown_transfer_encoding() {
        let mut buf = BytesMut::from(
            "GET / HTTP/1.1\r\n\
             Host: example.com\r\n\
             Transfer-Encoding: JUNK\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             5\r\n\
             hello\r\n\
             0",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_multiple_transfer_encoding() {
        let mut buf = BytesMut::from(
            "GET / HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 51\r\n\
             Transfer-Encoding: identity\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             0\r\n\
             \r\n\
             GET /forbidden HTTP/1.1\r\n\
             Host: example.com\r\n\r\n",
        );
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_transfer_encoding_content_length_combination() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 3\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             0\r\n",
        );
        expect_parse_err!(&mut buf);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             Host: example.com\r\n\
             Transfer-Encoding: chunked\r\n\
             Content-Length: 3\r\n\
             \r\n\
             0\r\n",
        );
        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_transfer_codings() {
        for (val, res) in [
            ("chunked", Some((true, false))),
            (" Chunked ", Some((true, false))),
            ("gzip, chunked", Some((true, true))),
            ("gzip;q=1 ,, chunked,", Some((true, true))),
            ("identity, chunked", Some((true, false))),
            ("identity", Some((false, false))),
            ("gzip", Some((false, true))),
            ("chunked, gzip", Some((false, true))),
            ("chunked, chunked", None),
            ("chunked, gzip, chunked", None),
            ("chunked;a=b", None),
            ("gz ip", None),
            (";a=b", None),
        ] {
            assert_eq!(transfer_codings(val.as_bytes()), res, "{val:?}");
        }
    }

    #[test]
    fn test_request_transfer_codings() {
        for (val, res) in [
            ("gzip, chunked", Err(DecodeError::UnsupportedTransferCoding)),
            ("chunked, gzip", Err(DecodeError::Header)),
            ("gzip", Err(DecodeError::Header)),
            ("chunked, chunked", Err(DecodeError::Header)),
            ("identity, chunked", Ok(())),
            ("chunked,", Ok(())),
        ] {
            let mut buf = BytesMut::from(
                format!("POST / HTTP/1.1\r\nhost: a\r\ntransfer-encoding: {val}\r\n\r\n").as_str(),
            );
            let reader = MessageDecoder::<Request>::default();
            let result = reader.decode(&mut buf).map(|msg| {
                let (req, pl) = msg.unwrap();
                assert!(req.chunked().unwrap());
                assert_eq!(pl, PayloadType::Payload(PayloadDecoder::chunked()));
            });
            assert_eq!(result, res, "{val:?}");
        }
    }

    #[test]
    fn test_response_transfer_codings() {
        // final chunked coding frames the payload, other codings are not decoded
        let mut buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: gzip, chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        );
        let reader = MessageDecoder::<ResponseHead>::default();
        let (res, pl) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(
            res.headers.get(header::TRANSFER_ENCODING).unwrap(),
            "gzip, chunked"
        );
        let pl = pl.unwrap();
        assert_eq!(
            pl.decode(&mut buf).unwrap(),
            Some(PayloadItem::Chunk("abc".into()))
        );
        assert_eq!(pl.decode(&mut buf).unwrap(), Some(PayloadItem::Eof));

        // without final chunked coding the payload is delimited by connection close
        for val in ["gzip", "chunked, gzip"] {
            let mut buf = BytesMut::from(
                format!("HTTP/1.1 200 OK\r\ntransfer-encoding: {val}\r\n\r\n3\r\nabc").as_str(),
            );
            let (res, pl) = reader.decode(&mut buf).unwrap().unwrap();
            assert_eq!(res.connection_type(), ConnectionType::Close, "{val:?}");
            let pl = pl.unwrap();
            assert!(pl.is_eof(), "{val:?}");
            assert_eq!(
                pl.decode(&mut buf).unwrap(),
                Some(PayloadItem::Chunk("3\r\nabc".into()))
            );
        }

        // codings with Content-Length are rejected in either order
        for hdrs in [
            "content-length: 3\r\ntransfer-encoding: gzip\r\n",
            "transfer-encoding: gzip\r\ncontent-length: 3\r\n",
            "transfer-encoding: gzip, chunked\r\ncontent-length: 3\r\n",
        ] {
            let mut buf = BytesMut::from(format!("HTTP/1.1 200 OK\r\n{hdrs}\r\nabc").as_str());
            let reader = MessageDecoder::<ResponseHead>::default();
            assert_eq!(
                reader.decode(&mut buf).err(),
                Some(DecodeError::Header),
                "{hdrs:?}"
            );
        }
    }

    #[test]
    fn test_transfer_encoding_identity() {
        for req in [
            "GET /test HTTP/1.1\r\nHost: a\r\n\
             Content-Length: 3\r\nTransfer-Encoding: identity\r\n\r\n0\r\n",
            "GET /test HTTP/1.1\r\nHost: a\r\n\
             Transfer-Encoding: identity\r\nContent-Length: 3\r\n\r\n0\r\n",
            "GET /test HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: identity\r\n\r\n",
            "GET /test HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: Identity \r\n\r\n",
        ] {
            let mut buf = BytesMut::from(req);
            let reader = MessageDecoder::<Request>::default();
            assert_eq!(
                reader.decode(&mut buf).err(),
                Some(DecodeError::Header),
                "{req:?}"
            );
        }

        // responses are tolerated
        let mut buf = BytesMut::from(
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\
             Transfer-Encoding: identity\r\n\r\n0\r\n",
        );
        let reader = MessageDecoder::<ResponseHead>::default();
        let (_msg, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        let chunk = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(chunk, PayloadItem::Chunk(Bytes::from_static(b"0\r\n")));
    }

    #[test]
    fn test_max_headers() {
        const TEXT: &str = "GET /test HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 3\r\n\
             Test-header: ****\r\n";

        const TEXT_2: &str = "GET /test HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 3\r\n\
             Test-header: ****\r\n\
             \r\n";

        let mut buf = BytesMut::from(TEXT);
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_buf_size(10))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let err = reader.decode(&mut buf).err().unwrap();
        assert_eq!(err, DecodeError::TooLarge(77));

        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_buf_size(100))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        // decode one message
        let mut buf = BytesMut::from(TEXT_2);
        let res = reader.decode(&mut buf);
        assert!(res.is_ok());

        // decode second message, same decoder
        let mut buf = BytesMut::from(TEXT);
        let res = reader.decode(&mut buf);
        assert!(res.is_ok());

        // MAX HEADERS
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_headers(1))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(TEXT);
        let err = reader.decode(&mut buf).err().unwrap();
        assert_eq!(err, DecodeError::MaxHeaders);
    }

    #[test]
    fn test_max_headers_repeated_names() {
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(
                HttpServiceConfig::new()
                    .set_max_headers(2)
                    .set_host_validation(false),
            )
            .into();

        // repeated names count separately
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from("GET / HTTP/1.1\r\nX: 1\r\nX: 2\r\nX: 3\r\n\r\n");
        assert_eq!(reader.decode(&mut buf).err(), Some(DecodeError::MaxHeaders));

        // count is kept across partial reads
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from("GET / HTTP/1.1\r\nX: 1\r\nX: 2\r\n");
        assert!(reader.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b"X: 3\r\n\r\n");
        assert_eq!(reader.decode(&mut buf).err(), Some(DecodeError::MaxHeaders));

        // count is reset for the next message
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(
            "GET / HTTP/1.1\r\nX: 1\r\nX: 2\r\n\r\nGET / HTTP/1.1\r\nX: 1\r\nX: 2\r\n\r\n",
        );
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.headers().get_all("x").count(), 2);
        assert!(reader.decode(&mut buf).unwrap().is_some());
    }

    #[test]
    fn test_decoder_reusable_after_error() {
        const VALID: &str = "GET /ok HTTP/1.1\r\nHost: example.com\r\n\r\n";

        let cfg: SharedCfg = SharedCfg::new("test")
            .add(
                HttpServiceConfig::new()
                    .set_max_headers(2)
                    .set_max_buf_size(128),
            )
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());

        let invalid = [
            // request line
            "G\x00T / HTTP/1.1\r\n\r\n".to_string(),
            // header
            "GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n".to_string(),
            // partial head followed by too many headers
            "GET / HTTP/1.1\r\nA: 1\r\nB: 2\r\nC: 3\r\n".to_string(),
            // message head is too large
            format!("GET / HTTP/1.1\r\nA: {}\r\n", "a".repeat(200)),
        ];
        for text in invalid {
            let mut buf = BytesMut::from(text.as_str());
            assert!(reader.decode(&mut buf).is_err(), "{text:?}");

            let mut buf = BytesMut::from(VALID);
            let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
            assert_eq!(req.path(), "/ok");
            assert_eq!(req.headers().len(), 1);
        }
    }

    #[test]
    fn test_max_buf_size_complete_message() {
        const TEXT: &str = "GET /test HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 3\r\n\
             Test-header: ****\r\n\
             \r\n";

        // whole message head is available in one buffer
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_buf_size(10))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(TEXT);
        let err = reader.decode(&mut buf).err().unwrap();
        assert_eq!(err, DecodeError::TooLarge(79));

        // message head completes on the second read
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_buf_size(78))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(&TEXT[..77]);
        assert!(reader.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&TEXT.as_bytes()[77..]);
        let err = reader.decode(&mut buf).err().unwrap();
        assert_eq!(err, DecodeError::TooLarge(79));

        // message head size is within the limit
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_buf_size(100))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(TEXT);
        assert!(reader.decode(&mut buf).unwrap().is_some());

        // the configured maximum is inclusive
        let cfg: SharedCfg = SharedCfg::new("test")
            .add(HttpServiceConfig::new().set_max_buf_size(TEXT.len()))
            .into();
        let reader = MessageDecoder::<Request>::new(cfg.get());
        let mut buf = BytesMut::from(TEXT);
        assert!(reader.decode(&mut buf).unwrap().is_some());
    }
}
