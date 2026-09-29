#![allow(
    unsafe_op_in_unsafe_fn,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_arguments
)]
use std::{cell::Cell, cmp, marker::PhantomData, ptr, slice};

use crate::http::config::DateService;
use crate::http::error::EncodeError;
use crate::http::header::{CONNECTION, CONTENT_LENGTH, DATE, HeaderName, TRANSFER_ENCODING, Value};
use crate::http::message::{ConnectionType, RequestHead};
use crate::http::{HeaderMap, Method, Response, StatusCode, Version, body::BodySize};
use crate::{util::BufMut, util::BytePages, util::Bytes};

#[derive(Debug)]
pub(crate) struct MessageEncoder<T: MessageType> {
    pub(super) length: BodySize,
    pub(super) te: Cell<TransferEncoding>,
    _t: PhantomData<T>,
}

impl<T: MessageType> Default for MessageEncoder<T> {
    fn default() -> Self {
        MessageEncoder {
            length: BodySize::None,
            te: Cell::new(TransferEncoding::empty()),
            _t: PhantomData,
        }
    }
}

impl<T: MessageType> Clone for MessageEncoder<T> {
    fn clone(&self) -> Self {
        MessageEncoder {
            length: self.length,
            te: self.te.clone(),
            _t: PhantomData,
        }
    }
}

pub(crate) trait MessageType: Sized {
    fn status(&self) -> Option<StatusCode>;

    fn headers(&self) -> &HeaderMap;

    fn chunked(&self) -> bool;

    fn encode_status(&self, dst: &mut BytePages);

    fn encode_headers(
        &self,
        dst: &mut BytePages,
        version: Version,
        mut length: BodySize,
        ctype: ConnectionType,
        extra_headers: Option<HeaderMap>,
    ) -> Result<(), EncodeError> {
        let chunked = self.chunked();
        let mut skip_len = length != BodySize::Stream;

        // Content length
        if let Some(status) = self.status() {
            if status == StatusCode::SWITCHING_PROTOCOLS {
                // no framing headers in 1xx responses, see RFC 9112 section 6.1
                skip_len = true;
                length = BodySize::None;
            } else if is_bodyless(status) {
                length = BodySize::None;
            }
        }
        match length {
            BodySize::None => dst.extend_from_slice(b"\r\n"),
            BodySize::Empty => dst.extend_from_slice(b"\r\ncontent-length: 0\r\n"),
            BodySize::Sized(len) => write_content_length(len, dst),
            BodySize::Stream => {
                if chunked {
                    skip_len = true;
                    dst.extend_from_slice(b"\r\ntransfer-encoding: chunked\r\n");
                } else {
                    skip_len = false;
                    dst.extend_from_slice(b"\r\n");
                }
            }
        }

        // Connection
        match ctype {
            ConnectionType::Upgrade => dst.extend_from_slice(b"connection: upgrade\r\n"),
            ConnectionType::KeepAlive if version < Version::HTTP_11 => {
                dst.extend_from_slice(b"connection: keep-alive\r\n");
            }
            // a response is sent as HTTP/1.1, which is persistent by default
            ConnectionType::Close if version >= Version::HTTP_11 || self.status().is_some() => {
                dst.extend_from_slice(b"connection: close\r\n");
            }
            _ => (),
        }

        // write headers, extra headers replace headers with the same name
        let mut has_date = false;
        let mut write = |key: &HeaderName, value: &Value| {
            match *key {
                CONNECTION => return,
                TRANSFER_ENCODING | CONTENT_LENGTH if skip_len => return,
                DATE => has_date = true,
                _ => (),
            }
            match value {
                Value::One(val) => put_header(dst, key.as_ref(), val.as_ref()),
                Value::Multi(vec) => {
                    for val in vec {
                        put_header(dst, key.as_ref(), val.as_ref());
                    }
                }
            }
        };
        if let Some(extra) = extra_headers.as_ref() {
            for (key, value) in self.headers().iter_inner() {
                if !extra.contains_key(key) {
                    write(key, value);
                }
            }
            for (key, value) in extra.iter_inner() {
                write(key, value);
            }
        } else {
            for (key, value) in self.headers().iter_inner() {
                write(key, value);
            }
        }

        // optimized date header, set_date writes \r\n, a request does not
        // need a date, see RFC 9110 section 6.6.1
        if has_date || self.status().is_none() {
            // msg eof
            dst.extend_from_slice(b"\r\n");
        } else {
            DateService.set_date_header2(dst);
        }

        Ok(())
    }
}

impl MessageType for Response<()> {
    fn status(&self) -> Option<StatusCode> {
        Some(self.head().status)
    }

    /// HTTP/1.0 does not support chunked transfer coding.
    fn chunked(&self) -> bool {
        self.head().chunked() && self.head().version >= Version::HTTP_11
    }

    fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    fn encode_status(&self, dst: &mut BytePages) {
        let head = self.head();

        // the highest supported version, see RFC 9110 section 2.5
        write_status_line(head.status, head.reason().as_bytes(), dst);
    }
}

impl MessageType for RequestHead {
    fn status(&self) -> Option<StatusCode> {
        None
    }

    /// HTTP/1.0 does not support chunked transfer coding.
    fn chunked(&self) -> bool {
        self.chunked() && self.version >= Version::HTTP_11
    }

    fn headers(&self) -> &HeaderMap {
        self.headers()
    }

    fn encode_status(&self, dst: &mut BytePages) {
        dst.put_slice(self.method.as_str().as_bytes());
        dst.put_u8(b' ');
        if let (&Method::CONNECT, Some(host)) = (&self.method, self.uri.host()) {
            // authority-form, see RFC 9112 section 3.2.3
            let port = self
                .uri
                .port_u16()
                .unwrap_or_else(|| match self.uri.scheme_str() {
                    Some("https" | "wss") => 443,
                    _ => 80,
                });
            dst.put_slice(host.as_bytes());
            dst.put_u8(b':');
            dst.put_slice(port.to_string().as_bytes());
        } else {
            dst.put_slice(
                self.uri
                    .path_and_query()
                    .map_or("/", |u| u.as_str())
                    .as_bytes(),
            );
        }
        dst.put_u8(b' ');
        dst.put_slice(
            // only HTTP-0.9/1.1
            match self.version {
                Version::HTTP_09 => b"HTTP/0.9",
                Version::HTTP_10 => b"HTTP/1.0",
                // Version::HTTP_11 => "HTTP/1.1",
                _ => b"HTTP/1.1",
            },
        );
    }
}

impl<T: MessageType> MessageEncoder<T> {
    /// Encode message
    pub(crate) fn encode_chunk(&self, msg: Bytes, buf: &mut BytePages) -> bool {
        let mut te = self.te.get();
        let result = te.encode(msg, buf);
        self.te.set(te);
        result
    }

    /// Encode eof
    pub(crate) fn encode_eof(&self, buf: &mut BytePages) -> Result<(), EncodeError> {
        let mut te = self.te.get();
        let result = te.encode_eof(buf);
        self.te.set(te);
        result
    }

    pub(crate) fn encode(
        &self,
        dst: &mut BytePages,
        message: &T,
        head: bool,
        stream: bool,
        version: Version,
        length: BodySize,
        ctype: ConnectionType,
        extra_headers: Option<HeaderMap>,
    ) -> Result<ConnectionType, EncodeError> {
        // a response with a bodyless status never sends body bytes
        let length = match message.status() {
            Some(status) if is_bodyless(status) => BodySize::None,
            // a response body without framing would be delimited by connection
            // close, see RFC 9112 section 6.3
            Some(status)
                if length == BodySize::None
                    && !head
                    && status != StatusCode::SWITCHING_PROTOCOLS =>
            {
                BodySize::Empty
            }
            _ => length,
        };

        // transfer encoding
        if head {
            self.te.set(TransferEncoding::empty());
        } else if message.status() == Some(StatusCode::SWITCHING_PROTOCOLS)
            && matches!(length, BodySize::Sized(_) | BodySize::Stream)
        {
            // a `101` response has no framing headers, its body belongs to
            // the new protocol, see RFC 9110 section 15.2.2
            self.te.set(TransferEncoding::eof());
        } else {
            self.te.set(match length {
                BodySize::Empty | BodySize::None => TransferEncoding::empty(),
                BodySize::Sized(len) => TransferEncoding::length(len),
                BodySize::Stream => {
                    if message.chunked() && !stream {
                        TransferEncoding::chunked()
                    } else {
                        TransferEncoding::eof()
                    }
                }
            });
        }

        // a request body cannot be delimited by connection close, it must have
        // a declared length, see RFC 9112 section 6.3
        if message.status().is_none()
            && self.te.get().kind == TransferEncodingKind::Eof
            && !message.headers().contains_key(CONTENT_LENGTH)
            && !extra_headers
                .as_ref()
                .is_some_and(|h| h.contains_key(CONTENT_LENGTH))
        {
            return Err(EncodeError::UnknownLength);
        }

        // a response body delimited by connection close ends the connection
        let ctype = if message.status().is_some()
            && !stream
            && self.te.get().kind == TransferEncodingKind::Eof
            && ctype == ConnectionType::KeepAlive
        {
            ConnectionType::Close
        } else {
            ctype
        };

        message.encode_status(dst);
        message.encode_headers(dst, version, length, ctype, extra_headers)?;
        Ok(ctype)
    }
}

/// Returns `true` for statuses that never have a response body:
/// informational (except `101 Switching Protocols`), `204 No Content`,
/// and `304 Not Modified`.
pub(super) fn is_bodyless(status: StatusCode) -> bool {
    status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
        || (status.is_informational() && status != StatusCode::SWITCHING_PROTOCOLS)
}

/// Encoders to handle different Transfer-Encodings.
#[derive(Debug, Copy, Clone)]
pub(super) struct TransferEncoding {
    kind: TransferEncodingKind,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum TransferEncodingKind {
    /// An Encoder for when Transfer-Encoding includes `chunked`.
    Chunked(bool),
    /// An Encoder for when Content-Length is set.
    ///
    /// Enforces that the body is not longer than the Content-Length header.
    Length(u64),
    /// An Encoder for when Content-Length is not known.
    ///
    /// Application decides when to stop writing.
    Eof,
}

impl TransferEncoding {
    #[inline]
    pub(super) fn empty() -> TransferEncoding {
        TransferEncoding {
            kind: TransferEncodingKind::Length(0),
        }
    }

    #[inline]
    pub(super) fn eof() -> TransferEncoding {
        TransferEncoding {
            kind: TransferEncodingKind::Eof,
        }
    }

    #[inline]
    pub(super) fn chunked() -> TransferEncoding {
        TransferEncoding {
            kind: TransferEncodingKind::Chunked(false),
        }
    }

    #[inline]
    pub(super) fn length(len: u64) -> TransferEncoding {
        TransferEncoding {
            kind: TransferEncodingKind::Length(len),
        }
    }

    /// Encode message. Return `EOF` state of encoder
    #[inline]
    pub(crate) fn encode(&mut self, mut msg: Bytes, buf: &mut BytePages) -> bool {
        match self.kind {
            TransferEncodingKind::Eof => {
                if msg.is_empty() {
                    true
                } else {
                    buf.append(msg);
                    false
                }
            }
            TransferEncodingKind::Chunked(eof) => {
                if eof {
                    return true;
                }

                // an empty chunk would be the last-chunk, only `encode_eof`
                // terminates the body
                if !msg.is_empty() {
                    write_chunk_size(msg.len(), buf);

                    buf.append(msg);
                    buf.extend_from_slice(b"\r\n");
                }
                false
            }
            TransferEncodingKind::Length(mut remaining) => {
                if remaining > 0 {
                    if msg.is_empty() {
                        return remaining == 0;
                    }
                    let len = cmp::min(remaining, msg.len() as u64);

                    buf.append(msg.split_to(len as usize));

                    remaining -= len;
                    self.kind = TransferEncodingKind::Length(remaining);
                    remaining == 0
                } else {
                    true
                }
            }
        }
    }

    /// Encode eof. Return `EOF` state of encoder
    #[inline]
    pub(crate) fn encode_eof(&mut self, buf: &mut BytePages) -> Result<(), EncodeError> {
        match self.kind {
            TransferEncodingKind::Eof => Ok(()),
            TransferEncodingKind::Length(rem) => {
                if rem != 0 {
                    Err(EncodeError::UnexpectedEof)
                } else {
                    Ok(())
                }
            }
            TransferEncodingKind::Chunked(eof) => {
                if !eof {
                    buf.extend_from_slice(b"0\r\n\r\n");
                    self.kind = TransferEncodingKind::Chunked(true);
                }
                Ok(())
            }
        }
    }
}

/// Writes a `name: value` header line.
#[inline]
fn put_header(dst: &mut BytePages, name: &[u8], value: &[u8]) {
    let len = name.len() + value.len() + 4;
    let spare = dst.chunk_mut();
    if spare.len() >= len {
        // SAFETY: the spare capacity of the current page holds `len` bytes
        unsafe {
            let p = spare.as_mut_ptr();
            ptr::copy_nonoverlapping(name.as_ptr(), p, name.len());
            let p = p.add(name.len());
            ptr::copy_nonoverlapping(b": ".as_ptr(), p, 2);
            let p = p.add(2);
            ptr::copy_nonoverlapping(value.as_ptr(), p, value.len());
            ptr::copy_nonoverlapping(b"\r\n".as_ptr(), p.add(value.len()), 2);
            dst.advance_mut(len);
        }
    } else {
        dst.put_slice(name);
        dst.put_slice(b": ");
        dst.put_slice(value);
        dst.put_slice(b"\r\n");
    }
}

const DEC_DIGITS_LUT: &[u8] = b"0001020304050607080910111213141516171819\
      2021222324252627282930313233343536373839\
      4041424344454647484950515253545556575859\
      6061626364656667686970717273747576777879\
      8081828384858687888990919293949596979899";

/// Writes `HTTP/1.1 <code> <reason>`.
fn write_status_line(status: StatusCode, reason: &[u8], dst: &mut BytePages) {
    let n = status.as_u16();
    let mut line = *b"HTTP/1.1 000 ";
    line[9] = b'0' + (n / 100) as u8;
    line[10] = b'0' + (n / 10 % 10) as u8;
    line[11] = b'0' + (n % 10) as u8;

    let len = line.len() + reason.len();
    let spare = dst.chunk_mut();
    if spare.len() >= len {
        // SAFETY: the spare capacity of the current page holds `len` bytes
        unsafe {
            let p = spare.as_mut_ptr();
            ptr::copy_nonoverlapping(line.as_ptr(), p, line.len());
            ptr::copy_nonoverlapping(reason.as_ptr(), p.add(line.len()), reason.len());
            dst.advance_mut(len);
        }
    } else {
        dst.put_slice(&line);
        dst.put_slice(reason);
    }
}

/// Writes the chunk size line, the size in uppercase hex and CRLF.
fn write_chunk_size(n: usize, bytes: &mut BytePages) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    // up to 16 digits for a 64-bit size and CRLF
    let mut buf = [0u8; 18];
    let digits = if n == 0 { 1 } else { n.ilog2() as usize / 4 + 1 };
    let mut n = n;
    for pos in (0..digits).rev() {
        buf[pos] = HEX[n & 0xf];
        n >>= 4;
    }
    buf[digits] = b'\r';
    buf[digits + 1] = b'\n';
    bytes.extend_from_slice(&buf[..digits + 2]);
}

/// NOTE: bytes object has to contain enough space
fn write_content_length(mut n: u64, bytes: &mut BytePages) {
    if n < 10 {
        let mut buf: [u8; 21] = *b"\r\ncontent-length: 0\r\n";
        buf[18] = (n as u8) + b'0';
        bytes.extend_from_slice(&buf);
    } else if n < 100 {
        let mut buf: [u8; 22] = *b"\r\ncontent-length: 00\r\n";
        let d1 = n << 1;
        unsafe {
            ptr::copy_nonoverlapping(
                DEC_DIGITS_LUT.as_ptr().add(d1 as usize),
                buf.as_mut_ptr().add(18),
                2,
            );
        }
        bytes.extend_from_slice(&buf);
    } else if n < 1000 {
        let mut buf: [u8; 23] = *b"\r\ncontent-length: 000\r\n";
        // decode 2 more chars, if > 2 chars
        let d1 = (n % 100) << 1;
        n /= 100;
        unsafe {
            ptr::copy_nonoverlapping(
                DEC_DIGITS_LUT.as_ptr().add(d1 as usize),
                buf.as_mut_ptr().add(19),
                2,
            );
        };

        // decode last 1
        buf[18] = (n as u8) + b'0';

        bytes.extend_from_slice(&buf);
    } else {
        bytes.extend_from_slice(b"\r\ncontent-length: ");
        convert_usize(n, bytes, true);
    }
}

pub(crate) fn convert_usize<B: BufMut>(mut n: u64, bytes: &mut B, eol: bool) {
    unsafe {
        let mut curr: isize = 39;
        let mut buf = [0u8; 41];
        buf[39] = b'\r';
        buf[40] = b'\n';
        let buf_ptr = buf.as_mut_ptr();
        let lut_ptr = DEC_DIGITS_LUT.as_ptr();

        // eagerly decode 4 characters at a time
        while n >= 10_000 {
            let rem = (n % 10_000) as isize;
            n /= 10_000;

            let d1 = (rem / 100) << 1;
            let d2 = (rem % 100) << 1;
            curr -= 4;
            ptr::copy_nonoverlapping(lut_ptr.offset(d1), buf_ptr.offset(curr), 2);
            ptr::copy_nonoverlapping(lut_ptr.offset(d2), buf_ptr.offset(curr + 2), 2);
        }

        // if we reach here numbers are <= 9999, so at most 4 chars long
        let mut n = n as isize; // possibly reduce 64bit math

        // decode 2 more chars, if > 2 chars
        if n >= 100 {
            let d1 = (n % 100) << 1;
            n /= 100;
            curr -= 2;
            ptr::copy_nonoverlapping(lut_ptr.offset(d1), buf_ptr.offset(curr), 2);
        }

        // decode last 1 or 2 chars
        if n < 10 {
            curr -= 1;
            *buf_ptr.offset(curr) = (n as u8) + b'0';
        } else {
            let d1 = n << 1;
            curr -= 2;
            ptr::copy_nonoverlapping(lut_ptr.offset(d1), buf_ptr.offset(curr), 2);
        }

        if eol {
            bytes.put_slice(slice::from_raw_parts(
                buf_ptr.offset(curr),
                41 - curr as usize,
            ));
        } else {
            bytes.put_slice(slice::from_raw_parts(
                buf_ptr.offset(curr),
                39 - curr as usize,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::RequestHead;
    use crate::http::header::{AUTHORIZATION, HeaderValue};

    #[test]
    fn test_chunked_te() {
        let mut bytes = BytePages::default();
        let mut enc = TransferEncoding::chunked();
        assert!(!enc.encode(b"test".into(), &mut bytes));
        // an empty chunk does not terminate the body
        assert!(!enc.encode(b"".into(), &mut bytes));
        assert!(!enc.encode(b"line".into(), &mut bytes));
        enc.encode_eof(&mut bytes).unwrap();
        assert!(enc.encode(b"late".into(), &mut bytes));

        let mut data = Vec::new();
        while let Some(chunk) = bytes.take() {
            data.extend_from_slice(&chunk);
        }
        assert_eq!(data, b"4\r\ntest\r\n4\r\nline\r\n0\r\n\r\n");
    }

    #[test]
    fn test_extra_headers() {
        let mut bytes = BytePages::default();

        let mut head = RequestHead::default();
        head.headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("some authorization"),
        );

        let mut extra_headers = HeaderMap::new();
        extra_headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("another authorization"),
        );
        extra_headers.insert(DATE, HeaderValue::from_static("date"));

        let _ = head.encode_headers(
            &mut bytes,
            Version::HTTP_11,
            BodySize::Empty,
            ConnectionType::Close,
            Some(extra_headers),
        );
        let data = String::from_utf8(Vec::from(bytes.take().unwrap().as_ref())).unwrap();
        assert!(data.contains("content-length: 0\r\n"));
        assert!(data.contains("connection: close\r\n"));
        assert!(data.contains("authorization: another authorization\r\n"));
        assert!(data.contains("date: date\r\n"));
    }

    #[test]
    fn test_connect_authority_form() {
        let encode = |method: Method, uri: &str| {
            let head = RequestHead {
                method,
                uri: uri.parse().unwrap(),
                ..Default::default()
            };
            let mut bytes = BytePages::default();
            head.encode_status(&mut bytes);
            String::from_utf8(Vec::from(bytes.take().unwrap().as_ref())).unwrap()
        };

        assert_eq!(
            encode(Method::CONNECT, "http://example.com:8080/path"),
            "CONNECT example.com:8080 HTTP/1.1"
        );
        assert_eq!(
            encode(Method::CONNECT, "https://example.com/"),
            "CONNECT example.com:443 HTTP/1.1"
        );
        assert_eq!(
            encode(Method::CONNECT, "http://example.com"),
            "CONNECT example.com:80 HTTP/1.1"
        );
        assert_eq!(
            encode(Method::CONNECT, "example.com:5000"),
            "CONNECT example.com:5000 HTTP/1.1"
        );
        assert_eq!(
            encode(Method::CONNECT, "http://[::1]:5000"),
            "CONNECT [::1]:5000 HTTP/1.1"
        );
        assert_eq!(
            encode(Method::GET, "http://example.com:8080/path?q=1"),
            "GET /path?q=1 HTTP/1.1"
        );
    }

    #[crate::rt_test]
    async fn test_request_without_date() {
        let mut bytes = BytePages::default();
        let head = RequestHead::default();
        let _ = head.encode_headers(
            &mut bytes,
            Version::HTTP_11,
            BodySize::None,
            ConnectionType::KeepAlive,
            None,
        );
        let data = String::from_utf8(bytes.take().unwrap().to_vec()).unwrap();
        assert!(!data.contains("date:"), "{data:?}");
        assert!(data.ends_with("\r\n\r\n"), "{data:?}");
    }

    #[crate::rt_test]
    async fn test_request_stream_framing() {
        let encode = |version: Version, chunking: bool, cl: bool| {
            let mut head = RequestHead {
                version,
                ..Default::default()
            };
            head.no_chunking(!chunking);
            if cl {
                head.headers
                    .insert(CONTENT_LENGTH, HeaderValue::from_static("4"));
            }
            let mut bytes = BytePages::default();
            let enc = MessageEncoder::<RequestHead>::default();
            enc.encode(
                &mut bytes,
                &head,
                false,
                false,
                version,
                BodySize::Stream,
                ConnectionType::KeepAlive,
                None,
            )
            .map(|_| String::from_utf8(bytes.take().unwrap().to_vec()).unwrap())
        };

        let data = encode(Version::HTTP_11, true, false).unwrap();
        assert!(data.contains("transfer-encoding: chunked\r\n"), "{data:?}");

        // HTTP/1.0 does not support chunked coding
        let data = encode(Version::HTTP_10, true, true).unwrap();
        assert!(!data.contains("transfer-encoding"), "{data:?}");
        assert!(data.contains("content-length: 4\r\n"), "{data:?}");
        assert!(matches!(
            encode(Version::HTTP_10, true, false),
            Err(EncodeError::UnknownLength)
        ));

        // no chunking, the body length must be declared
        let data = encode(Version::HTTP_11, false, true).unwrap();
        assert!(data.contains("content-length: 4\r\n"), "{data:?}");
        assert!(matches!(
            encode(Version::HTTP_11, false, false),
            Err(EncodeError::UnknownLength)
        ));
    }

    #[test]
    fn test_convert_usize() {
        for n in [0, 7, 42, 999, 10_000, 123_456_789, u64::MAX] {
            let mut b = BytePages::default();
            convert_usize(n, &mut b, false);
            assert_eq!(b.take().unwrap().as_ref(), n.to_string().as_bytes());

            convert_usize(n, &mut b, true);
            assert_eq!(b.take().unwrap().as_ref(), format!("{n}\r\n").as_bytes());
        }
    }

    #[test]
    fn test_write_chunk_size() {
        for n in [
            0,
            1,
            9,
            10,
            15,
            16,
            255,
            256,
            4095,
            4096,
            0x00AB_CDEF,
            usize::MAX,
        ] {
            let mut b = BytePages::default();
            write_chunk_size(n, &mut b);
            let mut data = Vec::new();
            while let Some(chunk) = b.take() {
                data.extend_from_slice(&chunk);
            }
            assert_eq!(data, format!("{n:X}\r\n").into_bytes());
        }
    }

    #[test]
    fn test_write_content_length() {
        let mut b = BytePages::default();

        write_content_length(0, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 0\r\n");
        write_content_length(9, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 9\r\n");
        write_content_length(10, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 10\r\n");
        write_content_length(99, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 99\r\n");
        write_content_length(100, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 100\r\n");
        write_content_length(101, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 101\r\n");
        write_content_length(998, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 998\r\n");
        write_content_length(1000, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 1000\r\n");
        write_content_length(1001, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 1001\r\n");
        write_content_length(5909, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 5909\r\n");
        write_content_length(25999, &mut b);
        assert_eq!(b.take().unwrap().as_ref(), b"\r\ncontent-length: 25999\r\n");
    }

    #[test]
    fn test_write_status_line() {
        for n in [100, 101, 200, 204, 404, 599, 999] {
            let status = StatusCode::from_u16(n).unwrap();
            let mut b = BytePages::default();
            write_status_line(status, b"Reason", &mut b);
            assert_eq!(
                b.take().unwrap().as_ref(),
                format!("HTTP/1.1 {n} Reason").as_bytes()
            );
        }
    }

    #[test]
    fn test_put_header_page_boundary() {
        use crate::util::BytePageSize;

        let mut b = BytePages::new(BytePageSize::Size4);
        b.put_u8(b'x');
        let spare = b.chunk_mut().len() + 1;

        // the header line ends before, exactly at and after the page end
        let line = b"x-name: value\r\n";
        for fill in spare - line.len() - 2..=spare {
            let mut b = BytePages::new(BytePageSize::Size4);
            b.extend_from_slice(&vec![b'.'; fill]);
            put_header(&mut b, b"x-name", b"value");
            write_status_line(StatusCode::OK, b"OK", &mut b);

            let mut data = Vec::new();
            while let Some(chunk) = b.take() {
                data.extend_from_slice(&chunk);
            }
            assert_eq!(&data[..fill], &vec![b'.'; fill][..]);
            assert_eq!(&data[fill..], b"x-name: value\r\nHTTP/1.1 200 OK");
        }
    }
}
