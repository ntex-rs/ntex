#![deny(warnings, rust_2018_idioms)]

use ntex_bytes::{BufMut, BytesMut};
use std::fmt::Write;

#[test]
fn test_vec_as_mut_buf() {
    let mut buf = Vec::with_capacity(64);

    assert_eq!(buf.remaining_mut(), usize::MAX);
    assert!(buf.chunk_mut().len() >= 64);

    buf.put(&b"zomg"[..]);

    assert_eq!(&buf, b"zomg");

    assert_eq!(buf.remaining_mut(), usize::MAX - 4);
    assert_eq!(buf.capacity(), 64);

    for _ in 0..16 {
        buf.put(&b"zomg"[..]);
    }

    assert_eq!(buf.len(), 68);
}

#[test]
fn test_put_u8() {
    let mut buf = Vec::with_capacity(8);
    buf.put_u8(33);
    assert_eq!(b"\x21", &buf[..]);
}

#[test]
fn test_put_u16() {
    let mut buf = Vec::with_capacity(8);
    buf.put_u16(8532);
    assert_eq!(b"\x21\x54", &buf[..]);

    buf.clear();
    buf.put_u16_le(8532);
    assert_eq!(b"\x54\x21", &buf[..]);
}

#[test]
fn test_vec_advance_mut() {
    let mut buf = Vec::with_capacity(8);
    unsafe {
        buf.chunk_mut()[0..8].copy_from_slice(b"12345678");
        buf.advance_mut(8);
    }
    assert_eq!(buf, b"12345678");
}

#[test]
#[should_panic(expected = "cannot advance past the capacity")]
fn test_vec_advance_mut_past_capacity() {
    let mut buf = Vec::<u8>::with_capacity(8);
    // the bytes past the capacity cannot have been initialized
    unsafe { buf.advance_mut(buf.capacity() + 1) };
}

#[test]
fn test_clone() {
    let mut buf = BytesMut::with_capacity(100);
    buf.write_str("this is a test").unwrap();
    let buf2 = buf.clone();

    buf.write_str(" of our emergency broadcast system").unwrap();
    assert!(buf != buf2);
}
