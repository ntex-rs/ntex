#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::unnecessary_mut_passed)]

use ntex_bytes::{Buf, Bytes, BytesMut};

#[test]
fn test_fresh_cursor_vec() {
    let mut buf = &b"hello"[..];

    assert_eq!(buf.remaining(), 5);
    assert_eq!(buf.chunk(), b"hello");

    buf.advance(2);

    assert_eq!(buf.remaining(), 3);
    assert_eq!(buf.chunk(), b"llo");

    buf.advance(3);

    assert_eq!(buf.remaining(), 0);
    assert_eq!(buf.chunk(), b"");
}

#[test]
fn test_bytes() {
    let mut buf = Bytes::from(b"hello");

    assert_eq!(bytes::buf::Buf::remaining(&buf), 5);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"hello");

    bytes::buf::Buf::advance(&mut buf, 2);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 3);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"llo");

    bytes::buf::Buf::advance(&mut buf, 3);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 0);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"");
}

#[test]
fn test_bytes_mut() {
    let mut buf = BytesMut::from(b"hello");

    assert_eq!(bytes::buf::Buf::remaining(&buf), 5);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"hello");

    assert_eq!(bytes::buf::BufMut::remaining_mut(&mut buf), 27);
    bytes::buf::Buf::advance(&mut buf, 2);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 3);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"llo");

    bytes::buf::Buf::advance(&mut buf, 3);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 0);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"");
}

#[test]
fn test_get_u8() {
    let mut buf = &b"\x21zomg"[..];
    assert_eq!(0x21, buf.get_u8());
}

#[test]
fn test_get_u16() {
    let mut buf = &b"\x21\x54zomg"[..];
    assert_eq!(0x2154, buf.get_u16());
    let mut buf = &b"\x21\x54zomg"[..];
    assert_eq!(0x5421, buf.get_u16_le());
}

#[cfg(not(target_os = "macos"))]
#[test]
#[should_panic]
fn test_get_u16_buffer_underflow() {
    let mut buf = &b"\x21"[..];
    buf.get_u16();
}
