#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::unnecessary_mut_passed)]

use ntex_bytes::{Buf, BufMut, Bytes, BytesMut};

/// STATEMENT: Slice-buffer advancement updates remaining bytes and reaches an empty chunk at exhaustion.
///
/// HOW:
/// - Advance a `hello` slice by two and then three, checking lengths five, three, and zero and matching chunks.
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

/// STATEMENT: The external `bytes::Buf` implementation consumes `Bytes` in the requested increments.
///
/// HOW:
/// - Advance `hello` by two and then three with `bytes::buf::Buf`, checking `hello`, `llo`, and empty chunks and lengths.
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

/// STATEMENT: Native buffer traits report initialized and spare capacity separately while consuming `BytesMut`.
///
/// HOW:
/// - Put `hello` in capacity 30, check 25 writable bytes, advance two and three, and verify remaining lengths and chunks.
#[test]
fn test_bytes_mut() {
    let mut buf = BytesMut::with_capacity(30);
    buf.extend_from_slice(b"hello");
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

/// STATEMENT: External buffer traits consume and refill `BytesMut` while exposing growable writable capacity.
///
/// HOW:
/// - Check `usize::MAX - 5` writable capacity, consume `hello`, and verify remaining chunks after each advancement.
/// - Write `12345` through slice, signed and unsigned byte, and raw chunk methods and check the final chunk.
#[test]
fn test_bytes_mut_buf() {
    let mut buf = BytesMut::with_capacity(30);
    buf.extend_from_slice(b"hello");

    assert_eq!(bytes::buf::Buf::remaining(&buf), 5);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"hello");

    assert_eq!(bytes::buf::BufMut::remaining_mut(&mut buf), usize::MAX - 5);
    bytes::buf::Buf::advance(&mut buf, 2);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 3);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"llo");

    bytes::buf::Buf::advance(&mut buf, 3);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 0);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"");

    bytes::buf::BufMut::put_slice(&mut buf, b"12");
    bytes::buf::BufMut::put_u8(&mut buf, b'3');
    bytes::buf::BufMut::put_i8(&mut buf, b'4' as i8);
    // SAFETY: The nonempty writable chunk receives one initialized byte before it is read and
    // advanced.
    unsafe {
        let c = bytes::buf::BufMut::chunk_mut(&mut buf);
        *c.as_mut_ptr() = b'5';
        assert!(c.as_mut_ptr().as_ref().unwrap() == &b'5');
        bytes::buf::BufMut::advance_mut(&mut buf, 1);
    }
    assert_eq!(&bytes::buf::Buf::chunk(&buf), b"12345");
}

/// STATEMENT: A copied mutable buffer reports no initial spare capacity and consumes bytes through the native `Buf` trait.
///
/// HOW:
/// - Copy `hello`, check zero writable bytes, advance by two and three, and compare chunks and lengths through exhaustion.
#[test]
fn test_bytes_vec() {
    let mut buf = BytesMut::from(b"hello");
    assert_eq!(Buf::remaining(&buf), 5);
    assert_eq!(Buf::chunk(&buf), b"hello");

    assert_eq!(BufMut::remaining_mut(&mut buf), 0);
    Buf::advance(&mut buf, 2);

    assert_eq!(Buf::remaining(&buf), 3);
    assert_eq!(Buf::chunk(&buf), b"llo");

    Buf::advance(&mut buf, 3);

    assert_eq!(Buf::remaining(&buf), 0);
    assert_eq!(Buf::chunk(&buf), b"");
}

/// STATEMENT: External buffer traits support consuming and refilling an exactly sized copied `BytesMut`.
///
/// HOW:
/// - Copy `hello`, check growable remaining capacity, and consume it in two advancements with expected chunks and lengths.
/// - Refill via slice, integer, and raw chunk writes and verify `12345`.
#[test]
fn test_bytes_vec_buf() {
    let mut buf = BytesMut::from(b"hello");

    assert_eq!(bytes::buf::Buf::remaining(&buf), 5);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"hello");

    assert_eq!(bytes::buf::BufMut::remaining_mut(&mut buf), usize::MAX - 5);
    bytes::buf::Buf::advance(&mut buf, 2);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 3);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"llo");

    bytes::buf::Buf::advance(&mut buf, 3);

    assert_eq!(bytes::buf::Buf::remaining(&buf), 0);
    assert_eq!(bytes::buf::Buf::chunk(&buf), b"");

    bytes::buf::BufMut::put_slice(&mut buf, b"12");
    bytes::buf::BufMut::put_u8(&mut buf, b'3');
    bytes::buf::BufMut::put_i8(&mut buf, b'4' as i8);
    // SAFETY: The nonempty writable chunk receives one initialized byte before it is read and
    // advanced.
    unsafe {
        let c = bytes::buf::BufMut::chunk_mut(&mut buf);
        *c.as_mut_ptr() = b'5';
        assert!(c.as_mut_ptr().as_ref().unwrap() == &b'5');
        bytes::buf::BufMut::advance_mut(&mut buf, 1);
    }
    assert_eq!(&bytes::buf::Buf::chunk(&buf), b"12345");
}

/// STATEMENT: External `bytes::BufMut` writers can grow a full buffer after checking its advertised writable capacity.
///
/// HOW:
/// - Encode 100 bytes after `x` using a capacity-checking encoder and append through a `BufMut` stream writer.
/// - Write a slice, chained chunks, and repeated bytes into another full buffer and check every resulting range and total length.
#[test]
fn test_bytes_mut_buf_grows() {
    fn encode(buf: &mut impl bytes::buf::BufMut, data: &[u8]) -> Result<(), ()> {
        if buf.remaining_mut() < data.len() {
            return Err(());
        }
        buf.put_slice(data);
        Ok(())
    }

    let mut buf = BytesMut::from(b"x");
    encode(&mut buf, &[1; 100]).unwrap();
    assert_eq!(&buf[1..], &[1; 100][..]);

    // a full buffer still hands out room to write into
    let mut buf = BytesMut::from(b"hello");
    assert!(bytes::buf::BufMut::chunk_mut(&mut buf).len() > 0);
    let mut w = bytes::buf::BufMut::writer(buf);
    std::io::Write::write_all(&mut w, b" world").unwrap();
    assert_eq!(w.into_inner(), b"hello world"[..]);

    let mut buf = BytesMut::from(b"a");
    bytes::buf::BufMut::put(&mut buf, &[2u8; 100][..]);
    bytes::buf::BufMut::put(&mut buf, bytes::Buf::chain(&b"bc"[..], &b"de"[..]));
    bytes::buf::BufMut::put_bytes(&mut buf, 3, 50);
    assert_eq!(buf.len(), 1 + 100 + 4 + 50);
    assert_eq!(&buf[..1], b"a");
    assert_eq!(&buf[1..101], &[2; 100][..]);
    assert_eq!(&buf[101..105], b"bcde");
    assert_eq!(&buf[105..], &[3; 50][..]);
}

/// STATEMENT: External mutable advancement still rejects counts beyond actual allocated capacity.
///
/// HOW:
/// - Allocate capacity eight, call `bytes::buf::BufMut::advance_mut` with capacity plus one, and expect the bounds panic.
#[test]
#[should_panic(expected = "cannot advance past `remaining_mut`")]
fn test_bytes_mut_buf_advance_past_capacity() {
    let mut buf = BytesMut::with_capacity(8);
    let cap = buf.capacity();
    // SAFETY: The implementation checks allocated capacity and panics before exposing any
    // uninitialized bytes.
    unsafe { bytes::buf::BufMut::advance_mut(&mut buf, cap + 1) };
}

/// STATEMENT: `get_u8` reads the leading byte value.
///
/// HOW:
/// - Read from a slice beginning with `0x21` followed by `zomg` and check the returned value is `0x21`.
#[test]
fn test_get_u8() {
    let mut buf = &b"\x21zomg"[..];
    assert_eq!(0x21, buf.get_u8());
}

/// STATEMENT: Sixteen-bit reads honor big- and little-endian byte order.
///
/// HOW:
/// - Read the leading bytes `0x21, 0x54` from separate slices and check `get_u16` yields `0x2154` and `get_u16_le` yields `0x5421`.
#[test]
fn test_get_u16() {
    let mut buf = &b"\x21\x54zomg"[..];
    assert_eq!(0x2154, buf.get_u16());
    let mut buf = &b"\x21\x54zomg"[..];
    assert_eq!(0x5421, buf.get_u16_le());
}

/// STATEMENT: Variable-width signed reads sign-extend their encoded width in both byte orders.
///
/// HOW:
/// - Decode explicit negative and positive byte sequences and verify a zero-byte read returns zero.
/// - Round-trip boundary and representative signed values at widths one through eight and compare with arithmetic sign extension.
#[test]
fn test_get_int_sign_extends() {
    let mut buf = &b"\xff\xfe\x80\x7f"[..];
    assert_eq!(-2, buf.get_int(2));
    assert_eq!(-128, buf.get_int(1));
    assert_eq!(127, buf.get_int(1));

    let mut buf = &b"\xfe\xff\x00\x80"[..];
    assert_eq!(-2, buf.get_int_le(2));
    assert_eq!(-32768, buf.get_int_le(2));

    assert_eq!(0, (&b""[..]).get_int(0));

    for n in [i64::MIN, -65536, -2, -1, 0, 1, 255, i64::MAX] {
        for nbytes in 1..=8 {
            let shift = (8 - nbytes) * 8;
            let expected = (n << shift) >> shift;

            let mut buf = BytesMut::new();
            buf.put_int(n, nbytes);
            buf.put_int_le(n, nbytes);
            assert_eq!(expected, buf.get_int(nbytes), "{n} {nbytes}");
            assert_eq!(expected, buf.get_int_le(nbytes), "{n} {nbytes}");
        }
    }
}

/// STATEMENT: Reading a sixteen-bit value from only one remaining byte panics.
///
/// HOW:
/// - On non-macOS targets, call `get_u16` on a one-byte slice and require the underflow panic.
#[cfg(not(target_os = "macos"))]
#[test]
#[should_panic]
fn test_get_u16_buffer_underflow() {
    let mut buf = &b"\x21"[..];
    buf.get_u16();
}
