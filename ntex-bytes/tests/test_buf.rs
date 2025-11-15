#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::unnecessary_mut_passed)]

use ntex_bytes::{Buf, BufMut, Bytes, BytesMut, BytesVec};

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
    assert_eq!(Buf::remaining(&buf), 5);
    assert_eq!(Buf::chunk(&buf), b"hello");

    assert_eq!(BufMut::remaining_mut(&mut buf), 25);
    Buf::advance(&mut buf, 2);

    assert_eq!(Buf::remaining(&buf), 3);
    assert_eq!(Buf::chunk(&buf), b"llo");

    Buf::advance(&mut buf, 3);

    assert_eq!(Buf::remaining(&buf), 0);
    assert_eq!(Buf::chunk(&buf), b"");
}

#[test]
fn test_bytes_mut_buf() {
    let mut buf = BytesMut::from(b"hello");

    assert_eq!(bytes::buf::Buf::remaining(&buf), 5);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"hello");

    assert_eq!(bytes::buf::BufMut::remaining_mut(&mut buf), 25);
    bytes::buf::Buf::advance(&mut buf, 2);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 3);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"llo");

    bytes::buf::Buf::advance(&mut buf, 3);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 0);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"");

    bytes::buf::BufMut::put_slice(&mut buf, b"12");
    bytes::buf::BufMut::put_u8(&mut buf, b'3');
    bytes::buf::BufMut::put_i8(&mut buf, b'4' as i8);
    unsafe {
        bytes::buf::BufMut::advance_mut(&mut buf, 2);
        let c = bytes::buf::BufMut::chunk_mut(&mut buf);
        *c.as_mut_ptr() = b'5';
        assert!(c.as_mut_ptr().as_ref().unwrap() == &b'5');
    }
    assert_eq!(&bytes::buf::Buf::chunk(&buf)[..4], b"1234");
}

#[test]
fn test_bytes_vec() {
    let mut buf = BytesVec::from(b"hello");
    assert_eq!(Buf::remaining(&buf), 5);
    assert_eq!(Buf::chunk(&buf), b"hello");

    assert_eq!(BufMut::remaining_mut(&mut buf), 43);
    Buf::advance(&mut buf, 2);

    assert_eq!(Buf::remaining(&buf), 3);
    assert_eq!(Buf::chunk(&buf), b"llo");

    Buf::advance(&mut buf, 3);

    assert_eq!(Buf::remaining(&buf), 0);
    assert_eq!(Buf::chunk(&buf), b"");
}

#[test]
fn test_bytes_vec_buf() {
    let mut buf = BytesVec::from(b"hello");

    assert_eq!(bytes::buf::Buf::remaining(&buf), 5);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"hello");

    assert_eq!(bytes::buf::BufMut::remaining_mut(&mut buf), 43);
    bytes::buf::Buf::advance(&mut buf, 2);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 3);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"llo");

    bytes::buf::Buf::advance(&mut buf, 3);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 0);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"");

    bytes::buf::BufMut::put_slice(&mut buf, b"12");
    bytes::buf::BufMut::put_u8(&mut buf, b'3');
    bytes::buf::BufMut::put_i8(&mut buf, b'4' as i8);
    unsafe {
        bytes::buf::BufMut::advance_mut(&mut buf, 2);
        let c = bytes::buf::BufMut::chunk_mut(&mut buf);
        *c.as_mut_ptr() = b'5';
        assert!(c.as_mut_ptr().as_ref().unwrap() == &b'5');
    }
    assert_eq!(&bytes::buf::Buf::chunk(&buf)[..4], b"1234");
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
