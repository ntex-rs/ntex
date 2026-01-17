#![allow(clippy::op_ref, clippy::let_underscore_future)]
use std::{borrow::Borrow, borrow::BorrowMut};

use ntex_bytes::info::Kind;
use ntex_bytes::{Buf, BufMut, Bytes, BytesMut};

const LONG: &[u8] = b"mary had a little lamb, little lamb, little lamb";
const SHORT: &[u8] = b"hello world";

fn inline_cap() -> usize {
    use std::mem;
    3 * mem::size_of::<usize>() - 1
}

fn is_sync<T: Sync>() {}
fn is_send<T: Send>() {}

#[cfg(target_pointer_width = "64")]
#[test]
fn test_size() {
    assert_eq!(24, std::mem::size_of::<Bytes>());
    assert_eq!(24, std::mem::size_of::<Option<Bytes>>());

    let mut t = BytesMut::new();
    t.extend_from_slice(&b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"[..]);

    let val = t.freeze();
    assert!(val.is_inline());

    let val = Some(val);
    assert!(val.is_some());
    assert_eq!(
        val.as_ref().unwrap(),
        &b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"[..]
    )
}

#[test]
fn test_bounds() {
    is_sync::<Bytes>();
    is_sync::<BytesMut>();
    is_send::<Bytes>();
    is_send::<BytesMut>();
}

#[test]
fn from_slice() {
    let a = Bytes::from(&b"abcdefgh"[..]);
    assert_eq!(a, b"abcdefgh"[..]);
    assert_eq!(a, &b"abcdefgh"[..]);
    assert_eq!(a, Vec::from(&b"abcdefgh"[..]));
    assert_eq!(b"abcdefgh"[..], a);
    assert_eq!(&b"abcdefgh"[..], a);
    assert_eq!(Vec::from(&b"abcdefgh"[..]), a);

    let a = BytesMut::copy_from_slice(&b"abcdefgh"[..]);
    assert_eq!(a, b"abcdefgh"[..]);
    assert_eq!(a, &b"abcdefgh"[..]);
    assert_eq!(b"abcdefgh"[..], a);
    assert_eq!(&b"abcdefgh"[..], a);
    assert_eq!(Vec::from(&b"abcdefgh"[..]), a);
}

#[test]
fn fmt() {
    let a = format!("{:?}", Bytes::from(&b"abcdefg"[..]));
    let b = "b\"abcdefg\"";

    assert_eq!(a, b);

    let a = format!("{:?}", BytesMut::copy_from_slice(&b"abcdefg"[..]));
    assert_eq!(a, b);
}

#[test]
fn clone_mut() {
    let buf1 = BytesMut::from("hello");

    let mut buf2 = buf1.clone();
    buf2[0] = b'x';

    assert_eq!(buf1, "hello");
}

#[allow(clippy::sliced_string_as_bytes)]
#[test]
fn fmt_write() {
    use std::fmt::Write;
    let s = String::from_iter((0..10).map(|_| "abcdefg"));

    let mut a = BytesMut::with_capacity(64);
    write!(a, "{}", &s[..64]).unwrap();
    assert_eq!(a, s[..64].as_bytes());

    let mut b = BytesMut::with_capacity(64);
    write!(b, "{}", &s[..32]).unwrap();
    write!(b, "{}", &s[32..64]).unwrap();
    assert_eq!(b, s[..64].as_bytes());

    let mut c = BytesMut::with_capacity(2);
    write!(c, "{s}").unwrap_err();
    assert!(c.is_empty());
}

#[test]
fn len() {
    let a = Bytes::from(&b"abcdefg"[..]);
    assert_eq!(a.len(), 7);

    let a = BytesMut::copy_from_slice(&b"abcdefg"[..]);
    assert_eq!(a.len(), 7);

    let a = Bytes::from(&b""[..]);
    assert!(a.is_empty());

    let a = BytesMut::copy_from_slice(&b""[..]);
    assert!(a.is_empty());
}

#[test]
fn inline() {
    let a = Bytes::from("abcdefg".to_string());
    assert!(a.is_inline());
    assert!(a.info().kind == Kind::Inline);

    let a = Bytes::from("".to_string());
    assert!(a.is_empty());

    let mut a = BytesMut::copy_from_slice(vec![b'*'; 35]).freeze();
    let b = a.split_to(8);
    assert!(b.is_inline());
}

#[test]
fn index() {
    let a = Bytes::from(&b"hello world"[..]);
    assert_eq!(a[0..5], *b"hello");

    let a = BytesMut::from(&b"hello world"[..]);
    assert_eq!(a[0..5], *b"hello");

    let a = BytesMut::copy_from_slice(&b"hello world"[..]);
    assert_eq!(a[0..5], *b"hello");
}

#[test]
fn slice() {
    let a = Bytes::from(&b"hello world"[..]);

    let b = a.slice(..);
    assert_eq!(b, b"hello world");

    let b = a.slice(3..5);
    assert_eq!(b, b"lo"[..]);

    let b = a.slice(3..=5);
    assert_eq!(b, b"lo "[..]);

    let b = a.slice(0..0);
    assert_eq!(b, b""[..]);

    let b = a.slice(3..3);
    assert_eq!(b, b""[..]);

    let b = a.slice(a.len()..a.len());
    assert_eq!(b, b""[..]);

    let b = a.slice(..5);
    assert_eq!(b, b"hello"[..]);

    let b = a.slice(3..);
    assert_eq!(b, b"lo world"[..]);
}

#[test]
#[should_panic]
fn slice_oob_1() {
    let a = Bytes::from(&b"hello world"[..]);
    a.slice(5..(inline_cap() + 1));
}

#[test]
#[should_panic]
fn slice_oob_2() {
    let a = Bytes::from(&b"hello world"[..]);
    a.slice((inline_cap() + 1)..(inline_cap() + 5));
}

#[test]
fn split_off() {
    let mut hello = Bytes::from(&b"helloworld"[..]);
    let world = hello.split_off(5);

    assert_eq!(hello, &b"hello"[..]);
    assert_eq!(world, &b"world"[..]);
}

#[test]
#[should_panic]
fn split_off_oob() {
    let mut hello = Bytes::from(&b"helloworld"[..]);
    hello.split_off(inline_cap() + 1);
}

const B: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

#[test]
fn split_off_to_loop() {
    for i in 0..(B.len() + 1) {
        {
            let mut bytes = Bytes::from(B);
            let off = bytes.split_off(i);
            assert_eq!(i, bytes.len());
            let mut sum: Vec<u8> = Vec::new();
            sum.extend(bytes.iter());
            sum.extend(off.iter());
            assert_eq!(B, &sum[..]);
        }
        {
            let mut bytes = Bytes::from(B.to_vec());
            let off = bytes.split_off(i);
            assert_eq!(i, bytes.len());
            let mut sum: Vec<u8> = Vec::new();
            sum.extend(bytes.iter());
            sum.extend(off.iter());
            assert_eq!(B, &sum[..]);
        }
        {
            let mut bytes = Bytes::from(B);
            let off = bytes.split_to(i);
            assert_eq!(i, off.len());
            let mut sum: Vec<u8> = Vec::new();
            sum.extend(off.iter());
            sum.extend(bytes.iter());
            assert_eq!(B, &sum[..]);
        }
        {
            let mut bytes = Bytes::from(B.to_vec());
            let off = bytes.split_to(i);
            assert_eq!(i, off.len());
            let mut sum: Vec<u8> = Vec::new();
            sum.extend(off.iter());
            sum.extend(bytes.iter());
            assert_eq!(B, &sum[..]);
        }
    }
}

#[test]
fn split_to_1() {
    // Inline
    let mut a = Bytes::from(SHORT);
    let b = a.split_to(4);

    assert_eq!(SHORT[4..], a);
    assert_eq!(SHORT[..4], b);

    // Allocated
    let mut a = Bytes::from(Vec::from(LONG));
    let b = a.split_to(4);

    assert_eq!(LONG[4..], a);
    assert_eq!(LONG[..4], b);

    let mut a = Bytes::from(Vec::from(LONG));
    let b = a.split_to(30);

    assert_eq!(LONG[30..], a);
    assert_eq!(LONG[..30], b);

    // bytes vec
    let mut a = BytesMut::copy_from_slice(LONG);
    let b = a.split_to(4);

    assert_eq!(LONG[4..], a);
    assert_eq!(LONG[..4], b);

    let mut a = BytesMut::copy_from_slice(LONG);
    let b = a.split_to(30);

    assert_eq!(LONG[30..], a);
    assert_eq!(LONG[..30], b);
}

#[test]
fn split_to_2() {
    let mut a = Bytes::from(LONG);
    assert_eq!(LONG, a);

    let b = a.split_to(1);

    assert_eq!(LONG[1..], a);
    drop(b);

    let mut a = BytesMut::copy_from_slice(LONG);
    assert_eq!(LONG, a);

    let b = a.split_to(1);

    assert_eq!(LONG[1..], a);
    drop(b);
}

#[test]
#[should_panic]
fn split_to_oob() {
    let mut hello = Bytes::from(&b"helloworld"[..]);
    hello.split_to(inline_cap() + 1);
}

#[test]
#[should_panic]
fn split_to_oob_mut() {
    let mut hello = BytesMut::copy_from_slice(&b"helloworld"[..]);
    hello.split_to(inline_cap() + 1);
}

#[test]
#[should_panic]
fn split_to_uninitialized() {
    let mut bytes = BytesMut::with_capacity(1024);
    let _other = bytes.split_to(128);
}

#[test]
fn split_off_to_at_gt_len() {
    fn make_bytes() -> Bytes {
        let mut bytes = BytesMut::with_capacity(100);
        bytes.put_slice(&[10, 20, 30, 40]);
        bytes.freeze()
    }

    use std::panic;

    make_bytes().split_to(4);
    make_bytes().split_off(4);

    assert!(
        panic::catch_unwind(move || {
            make_bytes().split_to(5);
        })
        .is_err()
    );

    assert!(
        panic::catch_unwind(move || {
            make_bytes().split_off(5);
        })
        .is_err()
    );
}

#[test]
#[allow(
    clippy::cmp_owned,
    clippy::redundant_slicing,
    clippy::iter_cloned_collect,
    clippy::needless_borrow
)]
fn fns_defined_for_bytes() {
    let bytes = Bytes::from(&b"hello world"[..]);
    let _ = bytes.as_ptr();
    assert_eq!(Borrow::<[u8]>::borrow(&bytes), b"hello world");

    let bytes = Bytes::from(b"hello world");
    assert_eq!(bytes, "hello world");
    assert_eq!(bytes, b"hello world");

    assert!(bytes > "g");
    assert!(bytes > b"g");
    assert!(bytes > [b'g']);
    assert!(bytes > &[b'g'][..]);
    assert!(bytes > "g".to_string());
    assert!(bytes > "g".as_bytes().to_vec());
    assert!(bytes > Bytes::from("g"));
    assert!("g" > bytes);
    assert!("g".to_string() > bytes);
    assert!("g".as_bytes().to_vec() > bytes);
    assert!([b'g'] > bytes);
    assert!(Bytes::from(&"g"[..]) < bytes);

    assert_eq!(bytes, "hello world");
    assert_eq!(bytes, "hello world".as_bytes().to_vec());
    assert_eq!(bytes, "hello world".to_string());
    assert_eq!(bytes, b"hello world");
    assert_eq!(bytes, &"hello world"[..]);
    assert_eq!(bytes, BytesMut::copy_from_slice(b"hello world"));
    assert_eq!(&bytes[..], b"hello world");
    assert_eq!(bytes.as_ref(), b"hello world");
    assert_eq!("hello world", bytes);
    assert_eq!("hello world".as_bytes().to_vec(), bytes);
    assert_eq!("hello world".to_string(), bytes);
    assert_eq!(b"hello world", bytes);
    assert_eq!(&"hello world"[..], bytes);
    assert_eq!(
        bytes,
        [
            b'h', b'e', b'l', b'l', b'o', b' ', b'w', b'o', b'r', b'l', b'd'
        ]
    );
    assert_eq!(
        [
            b'h', b'e', b'l', b'l', b'o', b' ', b'w', b'o', b'r', b'l', b'd'
        ],
        bytes,
    );

    let bytes = Bytes::copy_from_slice(&B[..]);
    assert!(bytes.info().kind == Kind::Vec);
    assert!(bytes.info().refs == 1);
    assert_eq!(bytes.info().capacity, 76);
    let b2 = bytes.clone();
    assert!(b2.info().kind == Kind::Vec);
    assert!(b2.info().refs == 2);
    assert!(b2.info().capacity == 76);
    drop(b2);
    assert!(bytes.info().kind == Kind::Vec);
    assert!(bytes.info().refs == 1);
    assert!(bytes.info().capacity == 76);

    let mut bytes = Bytes::from(&b"hello world"[..]);

    // Iterator
    let v: Vec<u8> = (&bytes).iter().cloned().collect();
    assert_eq!(&v[..], bytes);

    let v: Vec<u8> = bytes.as_ref().iter().cloned().collect();
    assert_eq!(&v[..], bytes);

    let v: Vec<u8> = bytes.clone().into_iter().collect();
    assert_eq!(&v[..], bytes);

    let v: Vec<u8> = (&bytes).into_iter().cloned().collect();
    assert_eq!(&v[..], bytes);

    let b2: Bytes = v.iter().collect();
    assert_eq!(b2, bytes);
    assert_eq!(&v[..], b2);

    bytes.truncate(5);
    assert_eq!(bytes, b"hello"[..]);

    bytes.clear();
    assert!(bytes.is_empty());
}

#[test]
#[allow(clippy::redundant_slicing)]
fn fns_defined_for_bytes_vec() {
    // BytesMut
    let mut bytes = BytesMut::copy_from_slice(&b"hello world"[..]);
    let _ = bytes.as_ptr();
    let _ = bytes.as_mut_ptr();
    assert_eq!(Borrow::<[u8]>::borrow(&bytes), b"hello world");
    assert_eq!(BorrowMut::<[u8]>::borrow_mut(&mut bytes), b"hello world");

    let mut bytes = BytesMut::copy_from_slice(b"hello world");
    assert_eq!(Borrow::<[u8]>::borrow(&bytes), b"hello world");
    assert_eq!(BorrowMut::<[u8]>::borrow_mut(&mut bytes), b"hello world");

    assert_eq!(bytes, "hello world");
    assert_eq!(bytes, "hello world".as_bytes().to_vec());
    assert_eq!(bytes, "hello world".to_string());
    assert_eq!(bytes, b"hello world");
    assert_eq!(bytes, &"hello world"[..]);
    assert_eq!(bytes, Bytes::copy_from_slice(b"hello world"));
    assert_eq!(&bytes[..], b"hello world");
    assert_eq!(
        bytes,
        [
            b'h', b'e', b'l', b'l', b'o', b' ', b'w', b'o', b'r', b'l', b'd'
        ]
    );
    assert_eq!("hello world", bytes);
    assert_eq!("hello world".as_bytes().to_vec(), bytes);
    assert_eq!("hello world".to_string(), bytes);
    assert_eq!(b"hello world", bytes);
    assert_eq!(
        [
            b'h', b'e', b'l', b'l', b'o', b' ', b'w', b'o', b'r', b'l', b'd'
        ],
        bytes
    );

    // Iterator
    let v: Vec<u8> = bytes.iter().cloned().collect();
    assert_eq!(&v[..], bytes);

    let v: Vec<u8> = bytes.as_ref().to_vec();
    assert_eq!(&v[..], bytes);

    let v: Vec<u8> = bytes.iter().cloned().collect();
    assert_eq!(&v[..], bytes);

    let v: Vec<u8> = (&bytes).into_iter().cloned().collect();
    assert_eq!(&v[..], bytes);

    let v: Vec<u8> = bytes.into_iter().collect();

    let mut bytes = BytesMut::copy_from_slice(b"hello world");
    assert_eq!(&v[..], bytes);

    let b2: BytesMut = v.iter().collect();
    assert_eq!(b2, bytes);
    assert_eq!(&v[..], b2);

    bytes.truncate(5);
    assert_eq!(bytes, b"hello"[..]);

    bytes.clear();
    assert!(bytes.is_empty());

    bytes.resize(10, b'1');
    assert_eq!(bytes, b"1111111111"[..]);

    assert_eq!(bytes.remaining(), 10);
    assert_eq!(bytes.chunk(), &b"1111111111"[..]);

    bytes.as_mut()[0] = b'2';
    assert_eq!(bytes, b"2111111111"[..]);

    let mut bytes = BytesMut::copy_from_slice(b"hello world");
    unsafe { bytes.set_len(1) };
    assert_eq!(bytes, "h");
}

#[test]
fn reserve_convert() {
    // Vec -> Vec
    let mut bytes = BytesMut::copy_from_slice(LONG);
    bytes.reserve(64);
    assert_eq!(bytes.capacity(), LONG.len() + 64);
}

#[test]
fn reserve_recycling() {
    let mut bytes = BytesMut::with_capacity(16);
    assert_eq!(bytes.capacity(), 16);
    bytes.put("0123456789012345".as_bytes());
    bytes.advance(10);
    assert_eq!(bytes.capacity(), 6);
    bytes.reserve(32);
    assert_eq!(bytes.capacity(), 38);
}

#[test]
fn extend_mut() {
    let mut bytes = BytesMut::with_capacity(0);
    bytes.extend(LONG);
    assert_eq!(*bytes, LONG[..]);
}

#[test]
fn extend_from_slice_mut() {
    for &i in &[3, 34] {
        let mut bytes = BytesMut::new();
        bytes.extend_from_slice(&LONG[..i]);
        bytes.extend_from_slice(&LONG[i..]);
        assert_eq!(LONG[..], *bytes);
    }
}

#[test]
fn from_static() {
    let mut a = Bytes::from_static(b"ab");
    let b = a.split_off(1);

    assert_eq!(a, b"a"[..]);
    assert_eq!(b, b"b"[..]);
}

#[test]
fn advance_inline() {
    let mut a = Bytes::from(&b"hello world"[..]);
    a.advance(6);
    assert_eq!(a, &b"world"[..]);
}

#[test]
fn advance_static() {
    let mut a = Bytes::from_static(b"hello world");
    a.advance(6);
    assert_eq!(a, &b"world"[..]);
}

#[test]
fn advance_vec() {
    let mut a = BytesMut::copy_from_slice(b"hello world boooo yah world zomg wat wat");
    a.advance(16);
    assert_eq!(a, b"o yah world zomg wat wat"[..]);

    a.advance(4);
    assert_eq!(a, b"h world zomg wat wat"[..]);

    // Reserve some space.
    a.reserve(1024);
    assert_eq!(a, b"h world zomg wat wat"[..]);

    a.advance(6);
    assert_eq!(a, b"d zomg wat wat"[..]);
}

#[test]
#[should_panic]
fn advance_past_len_vec() {
    let mut a = BytesMut::copy_from_slice(b"hello world");
    a.advance(20);
}

#[test]
fn partial_eq_bytesvec() {
    let bytes = Bytes::from(&b"The quick red fox"[..]);
    let bytesmut = BytesMut::copy_from_slice(&b"The quick red fox"[..]);
    assert!(bytes == bytesmut);
    assert!(bytesmut == bytes);
    let bytes2 = Bytes::from(&b"Jumped over the lazy brown dog"[..]);
    assert!(bytes2 != bytesmut);
    assert!(bytesmut != bytes2);
}

#[test]
fn from_iter_no_size_hint() {
    use std::iter;

    let mut expect = vec![];

    let actual: Bytes = iter::repeat(b'x')
        .scan(100, |cnt, item| {
            if *cnt >= 1 {
                *cnt -= 1;
                expect.push(item);
                Some(item)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(&actual[..], &expect[..]);
}

fn test_slice_ref(bytes: &Bytes, start: usize, end: usize, expected: &[u8]) {
    let slice = &(bytes.as_ref()[start..end]);
    let sub = bytes.slice_ref(slice);
    assert_eq!(&sub[..], expected);
}

#[test]
fn slice_ref_works() {
    let bytes = Bytes::from(&b"012345678"[..]);

    test_slice_ref(&bytes, 0, 0, b"");
    test_slice_ref(&bytes, 0, 3, b"012");
    test_slice_ref(&bytes, 2, 6, b"2345");
    test_slice_ref(&bytes, 7, 9, b"78");
    test_slice_ref(&bytes, 9, 9, b"");
}

#[test]
fn slice_ref_empty() {
    let bytes = Bytes::from(&b""[..]);
    let slice = &(bytes.as_ref()[0..0]);

    let sub = bytes.slice_ref(slice);
    assert_eq!(&sub[..], b"");
}

#[test]
#[should_panic]
fn slice_ref_catches_not_a_subset() {
    let bytes = Bytes::from(&b"012345678"[..]);
    let slice = &b"012345"[0..4];

    bytes.slice_ref(slice);
}

#[test]
#[should_panic]
fn slice_ref_catches_not_an_empty_subset() {
    let bytes = Bytes::from(&b"012345678"[..]);
    let slice = &b""[0..0];

    bytes.slice_ref(slice);
}

#[test]
#[should_panic]
fn empty_slice_ref_catches_not_an_empty_subset() {
    let bytes = Bytes::copy_from_slice(&b""[..]);
    let slice = &b""[0..0];

    bytes.slice_ref(slice);
}

#[test]
fn bytes_vec_freeze() {
    let bytes = BytesMut::copy_from_slice(b"12345");
    assert_eq!(bytes, &b"12345"[..]);
    let b = bytes.freeze();
    assert_eq!(b, &b"12345"[..]);
    assert!(b.is_inline());

    let bytes = BytesMut::copy_from_slice(LONG);
    assert_eq!(bytes, LONG);
    let b = bytes.freeze();
    assert_eq!(b, LONG);
}

#[test]
fn bytes_vec() {
    let mut bytes = BytesMut::from("12345");
    assert_eq!(bytes, "12345");
    assert_eq!("12345", bytes);
    let b: Bytes = bytes.split_to(3);
    assert_eq!(b, "123");
    assert_eq!("123", b);
    assert_eq!(bytes, "45");

    let data: [u8; 3] = [1, 2, 3];
    let bytes = BytesMut::from(data);
    assert_eq!(bytes, b"\x01\x02\x03");
}
