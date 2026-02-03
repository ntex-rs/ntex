#![allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
use std::marker::PhantomData;
use std::{cell::Cell, cmp, io::Write, mem, ptr, ptr::copy_nonoverlapping, slice};

use crate::http::body::BodySize;
use crate::http::config::DateService;
use crate::http::error::EncodeError;
use crate::http::header::{CONNECTION, CONTENT_LENGTH, DATE, TRANSFER_ENCODING, Value};
use crate::http::message::{ConnectionType, RequestHeadType};
use crate::http::{HeaderMap, Response, StatusCode, Version, helpers};
use crate::{io::IoConfig, util::BufMut, util::BytesMut};

#[derive(Debug)]
pub(super) struct MessageEncoder<T: MessageType> {
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

pub(super) trait MessageType: Sized {
    fn status(&self) -> Option<StatusCode>;

    fn headers(&self) -> &HeaderMap;

    fn extra_headers(&self) -> Option<&HeaderMap>;

    fn chunked(&self) -> bool;

    fn encode_status(&self, dst: &mut BytesMut) -> Result<(), EncodeError>;

    fn encode_headers(
        &self,
        dst: &mut BytesMut,
        version: Version,
        mut length: BodySize,
        ctype: ConnectionType,
        cfg: &IoConfig,
    ) -> Result<(), EncodeError> {
        let chunked = self.chunked();
        let mut skip_len = length != BodySize::Stream;

        // Content length
        if let Some(status) = self.status() {
            match status {
                StatusCode::NO_CONTENT | StatusCode::CONTINUE | StatusCode::PROCESSING => {
                    length = BodySize::None;
                }
                StatusCode::SWITCHING_PROTOCOLS => {
                    skip_len = true;
                    length = BodySize::Stream;
                }
                _ => (),
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
            ConnectionType::Close if version >= Version::HTTP_11 => {
                dst.extend_from_slice(b"connection: close\r\n");
            }
            _ => (),
        }

        // merging headers from head and extra headers. HeaderMap::new() does not allocate.
        let empty_headers = HeaderMap::new();
        let extra_headers = self.extra_headers().unwrap_or(&empty_headers);
        let headers = self
            .headers()
            .iter_inner()
            .filter(|(name, _)| !extra_headers.contains_key(*name))
            .chain(extra_headers.iter_inner());

        // write headers
        let mut pos = 0;
        let mut has_date = false;
        let mut remaining = dst.remaining_mut();
        let mut buf = dst.chunk_mut().as_mut_ptr();
        for (key, value) in headers {
            match *key {
                CONNECTION => continue,
                TRANSFER_ENCODING | CONTENT_LENGTH if skip_len => continue,
                DATE => {
                    has_date = true;
                }
                _ => (),
            }
            let k = key.as_str().as_bytes();
            match value {
                Value::One(val) => {
                    let v = val.as_ref();
                    let v_len = v.len();
                    let k_len = k.len();
                    let len = k_len + v_len + 4;

                    unsafe {
                        if len > remaining {
                            dst.advance_mut(pos);
                            pos = 0;
                            cfg.write_buf().resize_min(dst, len + len);
                            remaining = dst.remaining_mut();
                            buf = dst.chunk_mut().as_mut_ptr();
                        }
                        copy_nonoverlapping(k.as_ptr(), buf, k_len);
                        buf = buf.add(k_len);
                        copy_nonoverlapping(b": ".as_ptr(), buf, 2);
                        buf = buf.add(2);
                        copy_nonoverlapping(v.as_ptr(), buf, v_len);
                        buf = buf.add(v_len);
                        copy_nonoverlapping(b"\r\n".as_ptr(), buf, 2);
                        buf = buf.add(2);
                    }
                    pos += len;
                    remaining -= len;
                }
                Value::Multi(vec) => {
                    for val in vec {
                        let v = val.as_ref();
                        let v_len = v.len();
                        let k_len = k.len();
                        let len = k_len + v_len + 4;

                        unsafe {
                            if len > remaining {
                                dst.advance_mut(pos);
                                pos = 0;
                                cfg.write_buf().resize_min(dst, len + len);
                                remaining = dst.remaining_mut();
                                buf = dst.chunk_mut().as_mut_ptr();
                            }
                            copy_nonoverlapping(k.as_ptr(), buf, k_len);
                            buf = buf.add(k_len);
                            copy_nonoverlapping(b": ".as_ptr(), buf, 2);
                            buf = buf.add(2);
                            copy_nonoverlapping(v.as_ptr(), buf, v_len);
                            buf = buf.add(v_len);
                            copy_nonoverlapping(b"\r\n".as_ptr(), buf, 2);
                            buf = buf.add(2);
                        };
                        pos += len;
                        remaining -= len;
                    }
                }
            }
        }
        unsafe {
            dst.advance_mut(pos);
        }

        // optimized date header, set_date writes \r\n
        if has_date {
            // msg eof
            dst.extend_from_slice(b"\r\n");
        } else {
            DateService.set_date_header(dst);
        }

        Ok(())
    }
}

impl MessageType for Response<()> {
    fn status(&self) -> Option<StatusCode> {
        Some(self.head().status)
    }

    fn chunked(&self) -> bool {
        self.head().chunked()
    }

    fn headers(&self) -> &HeaderMap {
        &self.head().headers
    }

    fn extra_headers(&self) -> Option<&HeaderMap> {
        None
    }

    fn encode_status(&self, dst: &mut BytesMut) -> Result<(), EncodeError> {
        let head = self.head();
        let reason = head.reason().as_bytes();

        // status line
        write_status_line(head.version, head.status.as_u16(), dst);
        dst.extend_from_slice(reason);
        Ok(())
    }
}

impl MessageType for RequestHeadType {
    fn status(&self) -> Option<StatusCode> {
        None
    }

    fn chunked(&self) -> bool {
        self.as_ref().chunked()
    }

    fn headers(&self) -> &HeaderMap {
        self.as_ref().headers()
    }

    fn extra_headers(&self) -> Option<&HeaderMap> {
        self.extra_headers()
    }

    fn encode_status(&self, dst: &mut BytesMut) -> Result<(), EncodeError> {
        let head = self.as_ref();
        write!(
            helpers::Writer(dst),
            "{} {} {}",
            head.method,
            head.uri.path_and_query().map_or("/", |u| u.as_str()),
            // only HTTP-0.9/1.1
            match head.version {
                Version::HTTP_09 => "HTTP/0.9",
                Version::HTTP_10 => "HTTP/1.0",
                Version::HTTP_11 => "HTTP/1.1",
                _ => return Err(EncodeError::UnsupportedVersion(head.version)),
            }
        )
        .map_err(EncodeError::Fmt)
    }
}

impl<T: MessageType> MessageEncoder<T> {
    /// Encode message
    pub(super) fn encode_chunk(
        &self,
        msg: &[u8],
        buf: &mut BytesMut,
        cfg: &'static IoConfig,
    ) -> Result<bool, EncodeError> {
        let mut te = self.te.get();
        let result = te.encode(msg, buf, cfg);
        self.te.set(te);
        result
    }

    /// Encode eof
    pub(super) fn encode_eof(&self, buf: &mut BytesMut) -> Result<(), EncodeError> {
        let mut te = self.te.get();
        let result = te.encode_eof(buf);
        self.te.set(te);
        result
    }

    pub(super) fn encode(
        &self,
        dst: &mut BytesMut,
        message: &mut T,
        head: bool,
        stream: bool,
        version: Version,
        length: BodySize,
        ctype: ConnectionType,
        cfg: &IoConfig,
    ) -> Result<(), EncodeError> {
        // transfer encoding
        if head {
            self.te.set(TransferEncoding::empty());
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

        message.encode_status(dst)?;
        message.encode_headers(dst, version, length, ctype, cfg)
    }
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
    pub(super) fn encode(
        &mut self,
        msg: &[u8],
        buf: &mut BytesMut,
        cfg: &IoConfig,
    ) -> Result<bool, EncodeError> {
        match self.kind {
            TransferEncodingKind::Eof => {
                let eof = msg.is_empty();
                buf.extend_from_slice(msg);
                Ok(eof)
            }
            TransferEncodingKind::Chunked(eof) => {
                if eof {
                    return Ok(true);
                }

                let result = if msg.is_empty() {
                    buf.extend_from_slice(b"0\r\n\r\n");
                    self.kind = TransferEncodingKind::Chunked(true);
                    true
                } else {
                    writeln!(helpers::Writer(buf), "{:X}\r", msg.len())
                        .map_err(EncodeError::Fmt)?;

                    cfg.write_buf().resize_min(buf, msg.len() + 2);
                    buf.extend_from_slice(msg);
                    buf.extend_from_slice(b"\r\n");
                    false
                };
                Ok(result)
            }
            TransferEncodingKind::Length(mut remaining) => {
                if remaining > 0 {
                    if msg.is_empty() {
                        return Ok(remaining == 0);
                    }
                    let len = cmp::min(remaining, msg.len() as u64);

                    cfg.write_buf().resize_min(buf, len as usize);
                    buf.extend_from_slice(&msg[..len as usize]);

                    remaining -= len;
                    self.kind = TransferEncodingKind::Length(remaining);
                    Ok(remaining == 0)
                } else {
                    Ok(true)
                }
            }
        }
    }

    /// Encode eof. Return `EOF` state of encoder
    #[inline]
    pub(super) fn encode_eof(&mut self, buf: &mut BytesMut) -> Result<(), EncodeError> {
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

const DEC_DIGITS_LUT: &[u8] = b"0001020304050607080910111213141516171819\
      2021222324252627282930313233343536373839\
      4041424344454647484950515253545556575859\
      6061626364656667686970717273747576777879\
      8081828384858687888990919293949596979899";

const STATUS_LINE_BUF_SIZE: usize = 13;

#[allow(clippy::cast_possible_wrap)]
fn write_status_line(version: Version, mut n: u16, bytes: &mut BytesMut) {
    let mut buf: [u8; STATUS_LINE_BUF_SIZE] = match version {
        Version::HTTP_2 => *b"HTTP/2       ",
        Version::HTTP_10 => *b"HTTP/1.0     ",
        Version::HTTP_09 => *b"HTTP/0.9     ",
        _ => *b"HTTP/1.1     ",
    };

    let mut curr: isize = 12;
    let buf_ptr = buf.as_mut_ptr();
    let lut_ptr = DEC_DIGITS_LUT.as_ptr();
    let four = n > 999;

    // decode 2 more chars, if > 2 chars
    let d1 = (n % 100) << 1;
    n /= 100;
    curr -= 2;

    unsafe {
        ptr::copy_nonoverlapping(lut_ptr.offset(d1 as isize), buf_ptr.offset(curr), 2);

        // decode last 1 or 2 chars
        if n < 10 {
            curr -= 1;
            *buf_ptr.offset(curr) = (n as u8) + b'0';
        } else {
            let d1 = n << 1;
            curr -= 2;
            ptr::copy_nonoverlapping(lut_ptr.offset(d1 as isize), buf_ptr.offset(curr), 2);
        }
    }

    bytes.extend_from_slice(&buf);
    if four {
        bytes.put_u8(b' ');
    }
}

/// NOTE: bytes object has to contain enough space
fn write_content_length(mut n: u64, bytes: &mut BytesMut) {
    if n < 10 {
        let mut buf: [u8; 21] = [
            b'\r', b'\n', b'c', b'o', b'n', b't', b'e', b'n', b't', b'-', b'l', b'e', b'n',
            b'g', b't', b'h', b':', b' ', b'0', b'\r', b'\n',
        ];
        buf[18] = (n as u8) + b'0';
        bytes.extend_from_slice(&buf);
    } else if n < 100 {
        let mut buf: [u8; 22] = [
            b'\r', b'\n', b'c', b'o', b'n', b't', b'e', b'n', b't', b'-', b'l', b'e', b'n',
            b'g', b't', b'h', b':', b' ', b'0', b'0', b'\r', b'\n',
        ];
        let d1 = n << 1;
        unsafe {
            ptr::copy_nonoverlapping(
                DEC_DIGITS_LUT.as_ptr().add(d1 as usize),
                buf.as_mut_ptr().offset(18),
                2,
            );
        }
        bytes.extend_from_slice(&buf);
    } else if n < 1000 {
        let mut buf: [u8; 23] = [
            b'\r', b'\n', b'c', b'o', b'n', b't', b'e', b'n', b't', b'-', b'l', b'e', b'n',
            b'g', b't', b'h', b':', b' ', b'0', b'0', b'0', b'\r', b'\n',
        ];
        // decode 2 more chars, if > 2 chars
        let d1 = (n % 100) << 1;
        n /= 100;
        unsafe {
            ptr::copy_nonoverlapping(
                DEC_DIGITS_LUT.as_ptr().add(d1 as usize),
                buf.as_mut_ptr().offset(19),
                2,
            );
        };

        // decode last 1
        buf[18] = (n as u8) + b'0';

        bytes.extend_from_slice(&buf);
    } else {
        bytes.extend_from_slice(b"\r\ncontent-length: ");
        unsafe { convert_usize(n, bytes) };
    }
}

unsafe fn convert_usize(mut n: u64, bytes: &mut BytesMut) {
    let mut curr: isize = 39;
    #[allow(invalid_value, clippy::uninit_assumed_init)]
    let mut buf: [u8; 41] = mem::MaybeUninit::uninit().assume_init();
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

    bytes.extend_from_slice(slice::from_raw_parts(
        buf_ptr.offset(curr),
        41 - curr as usize,
    ));
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::*;
    use crate::http::RequestHead;
    use crate::http::header::{AUTHORIZATION, HeaderValue};
    use crate::util::Bytes;

    #[test]
    fn test_chunked_te() {
        let mut bytes = BytesMut::new();
        let mut enc = TransferEncoding::chunked();
        {
            assert!(
                !enc.encode(b"test", &mut bytes, &IoConfig::default())
                    .ok()
                    .unwrap()
            );
            assert!(
                enc.encode(b"", &mut bytes, &IoConfig::default())
                    .ok()
                    .unwrap()
            );
        }
        assert_eq!(bytes.take(), Bytes::from_static(b"4\r\ntest\r\n0\r\n\r\n"));
    }

    #[test]
    fn test_extra_headers() {
        let mut bytes = BytesMut::with_capacity(2048);

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

        let head = RequestHeadType::Rc(Rc::new(head), Some(extra_headers));

        let _ = head.encode_headers(
            &mut bytes,
            Version::HTTP_11,
            BodySize::Empty,
            ConnectionType::Close,
            &IoConfig::default(),
        );
        let data = String::from_utf8(Vec::from(bytes.take().as_ref())).unwrap();
        assert!(data.contains("content-length: 0\r\n"));
        assert!(data.contains("connection: close\r\n"));
        assert!(data.contains("authorization: another authorization\r\n"));
        assert!(data.contains("date: date\r\n"));
    }

    #[test]
    fn test_write_content_length() {
        let mut bytes = BytesMut::new();
        bytes.reserve(50);
        write_content_length(0, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 0\r\n"[..]);
        bytes.reserve(50);
        write_content_length(9, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 9\r\n"[..]);
        bytes.reserve(50);
        write_content_length(10, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 10\r\n"[..]);
        bytes.reserve(50);
        write_content_length(99, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 99\r\n"[..]);
        bytes.reserve(50);
        write_content_length(100, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 100\r\n"[..]);
        bytes.reserve(50);
        write_content_length(101, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 101\r\n"[..]);
        bytes.reserve(50);
        write_content_length(998, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 998\r\n"[..]);
        bytes.reserve(50);
        write_content_length(1000, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 1000\r\n"[..]);
        bytes.reserve(50);
        write_content_length(1001, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 1001\r\n"[..]);
        bytes.reserve(50);
        write_content_length(5909, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 5909\r\n"[..]);
        write_content_length(25999, &mut bytes);
        assert_eq!(bytes.take(), b"\r\ncontent-length: 25999\r\n"[..]);
    }
}
