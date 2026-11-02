#![deny(warnings, rust_2018_idioms)]

use ntex_bytes::{BufMut, BytesMut};
use std::fmt::Write;

/// STATEMENT: `Vec` supports `BufMut` writes and grows beyond its initial allocation.
///
/// HOW:
/// - Start with capacity 64, check writable space, append `zomg`, and verify contents, remaining space, and unchanged capacity.
/// - Append it sixteen more times and check length 68.
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

/// STATEMENT: `put_u8` appends the byte value to a vector.
///
/// HOW:
/// - Write 33 into a capacity-eight vector and compare its contents with the single byte `0x21`.
#[test]
fn test_put_u8() {
    let mut buf = Vec::with_capacity(8);
    buf.put_u8(33);
    assert_eq!(b"\x21", &buf[..]);
}

/// STATEMENT: `put_u16` and `put_u16_le` encode opposite byte orders.
///
/// HOW:
/// - Write 8532, compare with `0x21, 0x54`, clear the vector, and verify little-endian output reverses those bytes.
#[test]
fn test_put_u16() {
    let mut buf = Vec::with_capacity(8);
    buf.put_u16(8532);
    assert_eq!(b"\x21\x54", &buf[..]);

    buf.clear();
    buf.put_u16_le(8532);
    assert_eq!(b"\x54\x21", &buf[..]);
}

/// STATEMENT: `advance_mut` exposes initialized bytes written into vector spare capacity.
///
/// HOW:
/// - Copy `12345678` into the writable chunk, advance by eight, and compare the resulting vector contents.
#[test]
fn test_vec_advance_mut() {
    let mut buf = Vec::with_capacity(8);
    unsafe {
        buf.chunk_mut()[0..8].copy_from_slice(b"12345678");
        buf.advance_mut(8);
    }
    assert_eq!(buf, b"12345678");
}

/// STATEMENT: Vector mutable advancement rejects a length beyond allocated capacity.
///
/// HOW:
/// - Allocate a byte vector and call `advance_mut` with its capacity plus one, expecting the capacity error panic.
#[test]
#[should_panic(expected = "cannot advance past the capacity")]
fn test_vec_advance_mut_past_capacity() {
    let mut buf = Vec::<u8>::with_capacity(8);
    // the bytes past the capacity cannot have been initialized
    unsafe { buf.advance_mut(buf.capacity() + 1) };
}

/// STATEMENT: A cloned `BytesMut` is independent of later writes to its source.
///
/// HOW:
/// - Write a string, clone the buffer, append another string to the original, and verify the two buffers differ.
#[test]
fn test_clone() {
    let mut buf = BytesMut::with_capacity(100);
    buf.write_str("this is a test").unwrap();
    let buf2 = buf.clone();

    buf.write_str(" of our emergency broadcast system").unwrap();
    assert!(buf != buf2);
}
