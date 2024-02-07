use std::{cell::Cell, marker::PhantomData, mem, task::Poll};

use ntex_http::header::{HeaderName, HeaderValue};
use ntex_http::{header, Method, StatusCode, Uri, Version};

use crate::codec::Decoder;
use crate::http::error::DecodeError;
use crate::http::header::HeaderMap;
use crate::http::message::{ConnectionType, ResponseHead};
use crate::http::request::Request;
use crate::util::{Buf, Bytes, BytesMut};

use super::MAX_BUFFER_SIZE;

const MAX_HEADERS: usize = 96;

#[derive(Debug)]
/// Incoming messagd decoder
pub(super) struct MessageDecoder<T: MessageType>(PhantomData<T>);

#[derive(Debug, PartialEq, Eq)]
/// Incoming request type
pub enum PayloadType {
    None,
    Payload(PayloadDecoder),
    Stream(PayloadDecoder),
}

impl<T: MessageType> Default for MessageDecoder<T> {
    fn default() -> Self {
        MessageDecoder(PhantomData)
    }
}

impl<T: MessageType> Clone for MessageDecoder<T> {
    fn clone(&self) -> Self {
        MessageDecoder(PhantomData)
    }
}

impl<T: MessageType> Decoder for MessageDecoder<T> {
    type Item = (T, PayloadType);
    type Error = DecodeError;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        T::decode(src)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum PayloadLength {
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

    /// Returns true if variant is represents zero-length (not none) payload.
    fn is_zero(&self) -> bool {
        self == &ZERO
    }
}

pub(super) trait MessageType: Sized {
    fn set_connection_type(&mut self, ctype: Option<ConnectionType>);

    fn set_expect(&mut self);

    fn headers_mut(&mut self) -> &mut HeaderMap;

    fn decode(src: &mut BytesMut) -> Result<Option<(Self, PayloadType)>, DecodeError>;

    fn set_headers(
        &mut self,
        slice: &Bytes,
        version: Version,
        raw_headers: &[HeaderIndex],
    ) -> Result<PayloadLength, DecodeError> {
        let mut ka = None;
        let mut has_upgrade = false;
        let mut expect = false;
        let mut chunked = false;
        let mut seen_te = false;
        let mut content_length = None;

        {
            let headers = self.headers_mut();

            for idx in raw_headers.iter() {
                let name = HeaderName::from_bytes(&slice[idx.name.0..idx.name.1]).unwrap();

                // Unsafe: httparse check header value for valid utf-8
                let value = unsafe {
                    HeaderValue::from_shared_unchecked(
                        slice.slice(idx.value.0..idx.value.1),
                    )
                };
                match name {
                    header::CONTENT_LENGTH if content_length.is_some() || chunked => {
                        log::debug!("multiple Content-Length not allowed");
                        return Err(DecodeError::Header);
                    }
                    header::CONTENT_LENGTH => match value.to_str() {
                        Ok(s) if s.trim_start().starts_with('+') => {
                            log::debug!("illegal Content-Length: {:?}", s);
                            return Err(DecodeError::Header);
                        }
                        Ok(s) => {
                            if let Ok(len) = s.parse::<u64>() {
                                // accept 0 lengths here and remove them in `decode` after all
                                // headers have been processed to prevent request smuggling issues
                                content_length = Some(len);
                            } else {
                                log::debug!("illegal Content-Length: {:?}", s);
                                return Err(DecodeError::Header);
                            }
                        }
                        Err(_) => {
                            log::debug!("illegal Content-Length: {:?}", value);
                            return Err(DecodeError::Header);
                        }
                    },
                    // transfer-encoding
                    header::TRANSFER_ENCODING if seen_te => {
                        log::debug!("Transfer-Encoding header usage is not allowed");
                        return Err(DecodeError::Header);
                    }
                    header::TRANSFER_ENCODING if version == Version::HTTP_11 => {
                        seen_te = true;
                        if let Ok(s) = value.to_str().map(str::trim) {
                            if s.eq_ignore_ascii_case("chunked") && content_length.is_none()
                            {
                                chunked = true
                            } else if s.eq_ignore_ascii_case("identity") {
                                // allow silently since multiple TE headers are already checked
                            } else {
                                log::debug!("illegal Transfer-Encoding: {:?}", s);
                                return Err(DecodeError::Header);
                            }
                        } else {
                            return Err(DecodeError::Header);
                        }
                    }
                    // connection keep-alive state
                    header::CONNECTION => {
                        ka = if let Ok(conn) = value.to_str().map(|conn| conn.trim()) {
                            if conn.eq_ignore_ascii_case("keep-alive") {
                                Some(ConnectionType::KeepAlive)
                            } else if conn.eq_ignore_ascii_case("close") {
                                Some(ConnectionType::Close)
                            } else if conn.eq_ignore_ascii_case("upgrade") {
                                Some(ConnectionType::Upgrade)
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                    }
                    header::UPGRADE => {
                        has_upgrade = true;
                        // check content-length, some clients (dart)
                        // sends "content-length: 0" with websocket upgrade
                        if let Ok(val) = value.to_str().map(|val| val.trim()) {
                            if val.eq_ignore_ascii_case("websocket") {
                                content_length = None;
                            }
                        }
                    }
                    header::EXPECT => {
                        let bytes = value.as_bytes();
                        if bytes.len() >= 4 && &bytes[0..4] == b"100-" {
                            expect = true;
                        }
                    }
                    _ => (),
                }

                headers.append(name, value);
            }
        }
        self.set_connection_type(ka);
        if expect {
            self.set_expect()
        }

        // https://tools.ietf.org/html/rfc7230#section-3.3.3
        if chunked {
            // Chunked encoding
            Ok(PayloadLength::Payload(PayloadType::Payload(
                PayloadDecoder::chunked(),
            )))
        } else if let Some(len) = content_length {
            // Content-Length
            Ok(PayloadLength::Payload(PayloadType::Payload(
                PayloadDecoder::length(len),
            )))
        } else if has_upgrade {
            Ok(PayloadLength::Upgrade)
        } else {
            Ok(PayloadLength::None)
        }
    }
}

impl MessageType for Request {
    fn set_connection_type(&mut self, ctype: Option<ConnectionType>) {
        if let Some(ctype) = ctype {
            self.head_mut().set_connection_type(ctype);
        }
    }

    fn set_expect(&mut self) {
        self.head_mut().set_expect();
    }

    fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head_mut().headers
    }

    fn decode(src: &mut BytesMut) -> Result<Option<(Self, PayloadType)>, DecodeError> {
        let mut headers: [mem::MaybeUninit<HeaderIndex>; MAX_HEADERS] = uninit_array();

        let (len, method, uri, ver, headers) = {
            let mut parsed: [mem::MaybeUninit<httparse::Header<'_>>; MAX_HEADERS] =
                uninit_array();

            let mut req = httparse::Request::new(&mut []);

            match req.parse_with_uninit_headers(src, &mut parsed)? {
                httparse::Status::Complete(len) => {
                    let method = Method::from_bytes(req.method.unwrap().as_bytes())
                        .map_err(|_| DecodeError::Method)?;
                    let uri = Uri::try_from(req.path.unwrap())?;
                    let version = if req.version.unwrap() == 1 {
                        Version::HTTP_11
                    } else {
                        Version::HTTP_10
                    };

                    (
                        len,
                        method,
                        uri,
                        version,
                        HeaderIndex::record(src, req.headers, &mut headers),
                    )
                }
                httparse::Status::Partial => {
                    if src.len() >= MAX_BUFFER_SIZE {
                        trace!("MAX_BUFFER_SIZE unprocessed data reached, closing");
                        return Err(DecodeError::TooLarge(src.len()));
                    }
                    return Ok(None);
                }
            }
        };

        let mut msg = Request::new();

        // convert headers
        let mut length = msg.set_headers(&src.split_to(len).freeze(), ver, headers)?;

        // disallow HTTP/1.0 POST requests that do not contain a Content-Length headers
        // see https://datatracker.ietf.org/doc/html/rfc1945#section-7.2.2
        if ver == Version::HTTP_10 && method == Method::POST && length.is_none() {
            debug!("no Content-Length specified for HTTP/1.0 POST request");
            return Err(DecodeError::Header);
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
                msg.head_mut().set_upgrade();
                PayloadType::Stream(PayloadDecoder::eof())
            }
            PayloadLength::None => {
                if method == Method::CONNECT {
                    msg.head_mut().set_upgrade();
                    PayloadType::Stream(PayloadDecoder::eof())
                } else {
                    PayloadType::None
                }
            }
        };

        let head = msg.head_mut();
        head.uri = uri;
        head.method = method;
        head.version = ver;

        Ok(Some((msg, decoder)))
    }
}

impl MessageType for ResponseHead {
    fn set_connection_type(&mut self, ctype: Option<ConnectionType>) {
        if let Some(ctype) = ctype {
            ResponseHead::set_connection_type(self, ctype);
        }
    }

    fn set_expect(&mut self) {}

    fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    fn decode(src: &mut BytesMut) -> Result<Option<(Self, PayloadType)>, DecodeError> {
        let mut headers: [mem::MaybeUninit<HeaderIndex>; MAX_HEADERS] = uninit_array();

        let (len, ver, status, headers) = {
            let mut parsed: [mem::MaybeUninit<httparse::Header<'_>>; MAX_HEADERS] =
                uninit_array();

            let mut res = httparse::Response::new(&mut []);
            match httparse::ParserConfig::default().parse_response_with_uninit_headers(
                &mut res,
                src,
                &mut parsed,
            )? {
                httparse::Status::Complete(len) => {
                    let version = if res.version.unwrap() == 1 {
                        Version::HTTP_11
                    } else {
                        Version::HTTP_10
                    };
                    let status = StatusCode::from_u16(res.code.unwrap())
                        .map_err(|_| DecodeError::Status)?;

                    (
                        len,
                        version,
                        status,
                        HeaderIndex::record(src, res.headers, &mut headers),
                    )
                }
                httparse::Status::Partial => {
                    return if src.len() >= MAX_BUFFER_SIZE {
                        log::error!("MAX_BUFFER_SIZE unprocessed data reached, closing");
                        Err(DecodeError::TooLarge(src.len()))
                    } else {
                        Ok(None)
                    };
                }
            }
        };

        let mut msg = ResponseHead::new(status);
        msg.version = ver;

        // convert headers
        let mut length = msg.set_headers(&src.split_to(len).freeze(), ver, headers)?;

        // Remove CL value if 0 now that all headers and HTTP/1.0 special cases are processed.
        // Protects against some request smuggling attacks.
        // See https://github.com/actix/actix-web/issues/2767.
        if length.is_zero() {
            length = PayloadLength::None;
        }

        // message payload
        let decoder = if let PayloadLength::Payload(pl) = length {
            pl
        } else if status == StatusCode::SWITCHING_PROTOCOLS {
            // switching protocol or connect
            PayloadType::Stream(PayloadDecoder::eof())
        } else {
            // for HTTP/1.0 read to eof and close connection
            if msg.version == Version::HTTP_10 {
                msg.set_connection_type(ConnectionType::Close);
                PayloadType::Payload(PayloadDecoder::eof())
            } else {
                PayloadType::None
            }
        };

        Ok(Some((msg, decoder)))
    }
}

#[derive(Clone, Copy)]
pub(super) struct HeaderIndex {
    pub(super) name: (usize, usize),
    pub(super) value: (usize, usize),
}

impl HeaderIndex {
    pub(super) fn record<'a>(
        bytes: &[u8],
        headers: &[httparse::Header<'_>],
        indices: &'a mut [mem::MaybeUninit<HeaderIndex>],
    ) -> &'a [HeaderIndex] {
        let bytes_ptr = bytes.as_ptr() as usize;

        let init_len = headers
            .iter()
            .zip(indices.iter_mut())
            .map(|(header, indices)| {
                let name_start = header.name.as_ptr() as usize - bytes_ptr;
                let name_end = name_start + header.name.len();
                let value_start = header.value.as_ptr() as usize - bytes_ptr;
                let value_end = value_start + header.value.len();

                indices.write(HeaderIndex {
                    name: (name_start, name_end),
                    value: (value_start, value_end),
                })
            })
            .count();

        // SAFETY:
        //
        // The total initialized items are counted by iterator.
        unsafe {
            &*(&indices[..init_len] as *const [mem::MaybeUninit<HeaderIndex>]
                as *const [HeaderIndex])
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Http payload item
pub enum PayloadItem {
    Chunk(Bytes),
    Eof,
}

/// Decoders to handle different Transfer-Encodings.
///
/// If a message body does not include a Transfer-Encoding, it *should*
/// include a Content-Length header.
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
        PayloadDecoder {
            kind: Cell::new(Kind::Chunked(ChunkedState::Size, 0)),
        }
    }

    pub(super) fn eof() -> PayloadDecoder {
        PayloadDecoder {
            kind: Cell::new(Kind::Eof),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum Kind {
    /// A Reader used when a Content-Length header is passed with a positive
    /// integer.
    Length(u64),
    /// A Reader used when Transfer-Encoding is `chunked`.
    Chunked(ChunkedState, u64),
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

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum ChunkedState {
    Size,
    SizeLws,
    Extension,
    SizeLf,
    Body,
    BodyCr,
    BodyLf,
    EndCr,
    EndLf,
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
                        buf = src.split();
                        *remaining -= len;
                    } else {
                        buf = src.split_to(*remaining as usize);
                        *remaining = 0;
                    };
                    self.kind.set(kind);
                    log::trace!("Length read: {}", buf.len());
                    Ok(Some(PayloadItem::Chunk(buf.freeze())))
                }
            }
            Kind::Chunked(ref mut state, ref mut size) => {
                let result = loop {
                    let mut buf = None;
                    // advances the chunked state
                    *state = match state.step(src, size, &mut buf) {
                        Poll::Pending => break Ok(None),
                        Poll::Ready(Ok(state)) => state,
                        Poll::Ready(Err(e)) => break Err(e),
                    };

                    if *state == ChunkedState::End {
                        log::trace!("End of chunked stream");
                        break Ok(Some(PayloadItem::Eof));
                    }

                    if let Some(buf) = buf {
                        break Ok(Some(PayloadItem::Chunk(buf)));
                    }
                    if src.is_empty() {
                        break Ok(None);
                    }
                };
                self.kind.set(kind);
                result
            }
            Kind::Eof => {
                if src.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(PayloadItem::Chunk(src.split().freeze())))
                }
            }
        }
    }
}

macro_rules! byte (
    ($rdr:ident) => ({
        if $rdr.len() > 0 {
            let b = $rdr[0];
            $rdr.advance(1);
            b
        } else {
            return Poll::Pending
        }
    })
);

impl ChunkedState {
    fn step(
        &self,
        body: &mut BytesMut,
        size: &mut u64,
        buf: &mut Option<Bytes>,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        use self::ChunkedState::*;
        match *self {
            Size => ChunkedState::read_size(body, size),
            SizeLws => ChunkedState::read_size_lws(body),
            Extension => ChunkedState::read_extension(body),
            SizeLf => ChunkedState::read_size_lf(body, size),
            Body => ChunkedState::read_body(body, size, buf),
            BodyCr => ChunkedState::read_body_cr(body),
            BodyLf => ChunkedState::read_body_lf(body),
            EndCr => ChunkedState::read_end_cr(body),
            EndLf => ChunkedState::read_end_lf(body),
            End => Poll::Ready(Ok(ChunkedState::End)),
        }
    }

    fn read_size(
        rdr: &mut BytesMut,
        size: &mut u64,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        let rem = match byte!(rdr) {
            b @ b'0'..=b'9' => b - b'0',
            b @ b'a'..=b'f' => b + 10 - b'a',
            b @ b'A'..=b'F' => b + 10 - b'A',
            b'\t' | b' ' => return Poll::Ready(Ok(ChunkedState::SizeLws)),
            b';' => return Poll::Ready(Ok(ChunkedState::Extension)),
            b'\r' => return Poll::Ready(Ok(ChunkedState::SizeLf)),
            _ => {
                return Poll::Ready(Err(DecodeError::InvalidInput(
                    "Invalid chunk size line: Invalid Size",
                )));
            }
        };

        match size.checked_mul(16) {
            Some(n) => {
                *size = n;
                *size += rem as u64;

                Poll::Ready(Ok(ChunkedState::Size))
            }
            None => {
                log::debug!("chunk size would overflow u64");
                Poll::Ready(Err(DecodeError::InvalidInput(
                    "Invalid chunk size line: Size is too big",
                )))
            }
        }
    }

    fn read_size_lws(rdr: &mut BytesMut) -> Poll<Result<ChunkedState, DecodeError>> {
        log::trace!("read_size_lws");
        match byte!(rdr) {
            // LWS can follow the chunk size, but no more digits can come
            b'\t' | b' ' => Poll::Ready(Ok(ChunkedState::SizeLws)),
            b';' => Poll::Ready(Ok(ChunkedState::Extension)),
            b'\r' => Poll::Ready(Ok(ChunkedState::SizeLf)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput(
                "Invalid chunk size linear white space",
            ))),
        }
    }
    fn read_extension(rdr: &mut BytesMut) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\r' => Poll::Ready(Ok(ChunkedState::SizeLf)),
            // strictly 0x20 (space) should be disallowed but we don't parse quoted strings here
            0x00..=0x08 | 0x0a..=0x1f | 0x7f => Poll::Ready(Err(
                DecodeError::InvalidInput("Invalid character in chunk extension"),
            )),
            _ => Poll::Ready(Ok(ChunkedState::Extension)), // no supported extensions
        }
    }
    fn read_size_lf(
        rdr: &mut BytesMut,
        size: &mut u64,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\n' if *size > 0 => Poll::Ready(Ok(ChunkedState::Body)),
            b'\n' if *size == 0 => Poll::Ready(Ok(ChunkedState::EndCr)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput("Invalid chunk size LF"))),
        }
    }

    fn read_body(
        rdr: &mut BytesMut,
        rem: &mut u64,
        buf: &mut Option<Bytes>,
    ) -> Poll<Result<ChunkedState, DecodeError>> {
        log::trace!("Chunked read, remaining={:?}", rem);

        let len = rdr.len() as u64;
        if len == 0 {
            Poll::Ready(Ok(ChunkedState::Body))
        } else {
            let slice;
            if *rem > len {
                slice = rdr.split();
                *rem -= len;
            } else {
                slice = rdr.split_to(*rem as usize);
                *rem = 0;
            }
            *buf = Some(slice.freeze());
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
    fn read_end_cr(rdr: &mut BytesMut) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\r' => Poll::Ready(Ok(ChunkedState::EndLf)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput("Invalid chunk end CR"))),
        }
    }
    fn read_end_lf(rdr: &mut BytesMut) -> Poll<Result<ChunkedState, DecodeError>> {
        match byte!(rdr) {
            b'\n' => Poll::Ready(Ok(ChunkedState::End)),
            _ => Poll::Ready(Err(DecodeError::InvalidInput("Invalid chunk end LF"))),
        }
    }
}

fn uninit_array<T, const LEN: usize>() -> [mem::MaybeUninit<T>; LEN] {
    // SAFETY: An uninitialized `[mem::MaybeUninit<_>; LEN]` is valid.
    unsafe { mem::MaybeUninit::uninit().assume_init() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header::{HeaderName, SET_COOKIE};
    use crate::http::{HttpMessage, Method, Version};
    use crate::util::{Bytes, BytesMut};

    impl PayloadType {
        fn unwrap(self) -> PayloadDecoder {
            match self {
                PayloadType::Payload(pl) => pl,
                _ => panic!(),
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
                _ => panic!("error"),
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
    fn test_parse() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n\r\n");

        let reader = MessageDecoder::<Request>::default();
        match reader.decode(&mut buf) {
            Ok(Some((req, _))) => {
                assert_eq!(req.version(), Version::HTTP_11);
                assert_eq!(*req.method(), Method::GET);
                assert_eq!(req.path(), "/test");
            }
            Ok(_) | Err(_) => unreachable!("Error during parsing http request"),
        }
    }

    #[test]
    fn test_parse_partial() {
        let mut buf = BytesMut::from("PUT /test HTTP/1");

        let reader = MessageDecoder::<Request>::default();
        assert!(reader.decode(&mut buf).unwrap().is_none());

        buf.extend(b".1\r\n\r\n");
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_11);
        assert_eq!(*req.method(), Method::PUT);
        assert_eq!(req.path(), "/test");
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
            Content-Length: 0\r\n\
            \r\n",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test2");

        let mut buf = BytesMut::from(
            "GET /test3 HTTP/1.0\r\n\
            Content-Length: 3\r\n\
            \r\n
            abc",
        );

        let reader = MessageDecoder::<Request>::default();
        let (req, _) = reader.decode(&mut buf).unwrap().unwrap();
        assert_eq!(req.version(), Version::HTTP_10);
        assert_eq!(*req.method(), Method::GET);
        assert_eq!(req.path(), "/test3");
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
        assert!(err.to_string().contains("Header"))
    }

    #[test]
    fn test_parse_body() {
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody");

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
        let mut buf =
            BytesMut::from("\r\nGET /test HTTP/1.1\r\nContent-Length: 4\r\n\r\nbody");

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
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n");
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
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n");

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
            "GET /test HTTP/1.1\r\n\
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
        let mut buf = BytesMut::from("GET /test HTTP/1.1\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
    }

    #[test]
    fn test_conn_close() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             connection: close\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::Close);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
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
            "GET /test HTTP/1.1\r\n\
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
            "GET /test HTTP/1.1\r\n\
             connection: other\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert_eq!(req.head().connection_type(), ConnectionType::KeepAlive);
    }

    #[test]
    fn test_conn_upgrade() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             upgrade: websockets\r\n\
             connection: upgrade\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
        assert_eq!(req.head().connection_type(), ConnectionType::Upgrade);

        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             upgrade: Websockets\r\n\
             connection: Upgrade\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
        assert_eq!(req.head().connection_type(), ConnectionType::Upgrade);
    }

    #[test]
    fn test_conn_upgrade_connect_method() {
        let mut buf = BytesMut::from(
            "CONNECT /test HTTP/1.1\r\n\
             content-type: text/plain\r\n\r\n",
        );
        let req = parse_ready!(&mut buf);

        assert!(req.upgrade());
    }

    #[test]
    fn test_request_chunked() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
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
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chnked\r\n\r\n",
        );
        expect_parse_err!(&mut buf)
    }

    #[test]
    fn test_headers_content_length_err_1() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             content-length: line\r\n\r\n",
        );

        expect_parse_err!(&mut buf)
    }

    #[test]
    fn test_headers_content_length_err_2() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             content-length: -1\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_invalid_header() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             test line\r\n\r\n",
        );

        expect_parse_err!(&mut buf);
    }

    #[test]
    fn test_invalid_name() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
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
            "GET /test HTTP/1.1\r\n\
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
    fn test_http_request_parser_utf8() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
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
        let mut buf = BytesMut::from("GET //path HTTP/1.1\r\n\r\n");
        let req = parse_ready!(&mut buf);

        assert_eq!(req.path(), "//path");
    }

    #[test]
    fn test_http_request_parser_bad_method() {
        let mut buf = BytesMut::from("!12%()+=~$ /get HTTP/1.1\r\n\r\n");

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
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n");
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"data"
        );
        assert_eq!(
            pl.decode(&mut buf).unwrap().unwrap().chunk().as_ref(),
            b"line"
        );
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_http_request_chunked_payload_and_next_message() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             transfer-encoding: chunked\r\n\r\n",
        );
        let reader = MessageDecoder::<Request>::default();
        let (req, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert!(req.chunked().unwrap());

        buf.extend(
            b"4\r\ndata\r\n4\r\nline\r\n0\r\n\r\n\
              POST /test2 HTTP/1.1\r\n\
              transfer-encoding: chunked\r\n\r\n"
                .iter(),
        );
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"data");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"line");
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
            "GET /test HTTP/1.1\r\n\
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

        //trailers
        //buf.feed_data("test: test\r\n");
        //not_ready!(reader.parse(&mut buf, &mut readbuf));

        buf.extend(b"ne\r\n0\r\n");
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(msg.chunk().as_ref(), b"ne");
        assert!(pl.decode(&mut buf).unwrap().is_none());

        buf.extend(b"\r\n");
        assert!(pl.decode(&mut buf).unwrap().unwrap().eof());
    }

    #[test]
    fn test_parse_chunked_payload_chunk_extension() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
              transfer-encoding: chunked\r\n\r\n",
        );

        let reader = MessageDecoder::<Request>::default();
        let (msg, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();
        assert!(msg.chunked().unwrap());

        buf.extend(b"4;test\r\ndata\r\n4\r\nline\r\n0\r\n\r\n"); // test: test\r\n\r\n")
        let chunk = pl.decode(&mut buf).unwrap().unwrap().chunk();
        assert_eq!(chunk, Bytes::from_static(b"data"));
        let chunk = pl.decode(&mut buf).unwrap().unwrap().chunk();
        assert_eq!(chunk, Bytes::from_static(b"line"));
        let msg = pl.decode(&mut buf).unwrap().unwrap();
        assert!(msg.eof());
    }

    #[test]
    fn test_response_http10_read_until_eof() {
        let mut buf = BytesMut::from("HTTP/1.0 200 Ok\r\n\r\ntest data");

        let reader = MessageDecoder::<ResponseHead>::default();
        let (_msg, pl) = reader.decode(&mut buf).unwrap().unwrap();
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
        // in HTTP/1.0 transfer encoding is ignored and must therefore contain a CL header

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
        // in HTTP/1.0 transfer encoding is simply ignored so it's fine to have both

        let mut buf = BytesMut::from(
            "GET / HTTP/1.0\r\n\
            Host: example.com\r\n\
            Content-Length: 3\r\n\
            Transfer-Encoding: chunked\r\n\
            \r\n\
            000",
        );

        parse_ready!(&mut buf);
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
    fn test_transfer_encoding_content_length() {
        let mut buf = BytesMut::from(
            "GET /test HTTP/1.1\r\n\
             Host: example.com\r\n\
             Content-Length: 3\r\n\
             Transfer-Encoding: identity\r\n\
             \r\n\
             0\r\n",
        );

        let reader = MessageDecoder::<Request>::default();
        let (_msg, pl) = reader.decode(&mut buf).unwrap().unwrap();
        let pl = pl.unwrap();

        let chunk = pl.decode(&mut buf).unwrap().unwrap();
        assert_eq!(chunk, PayloadItem::Chunk(Bytes::from_static(b"0\r\n")));
    }
}
