#![deny(warnings, rust_2018_idioms)]

use ntex_bytes::Bytes;

/// STATEMENT: A nonempty byte iterator reports its exact length and size hint.
///
/// HOW:
/// - Iterate static `hello world` and check length 11 and the size hint `(11, Some(11))`.
#[test]
fn iter_len() {
    let buf = Bytes::from_static(b"hello world");
    let iter = buf.iter();

    assert_eq!(iter.size_hint(), (11, Some(11)));
    assert_eq!(iter.len(), 11);
}

/// STATEMENT: An empty byte iterator reports zero as both length and size-hint bounds.
///
/// HOW:
/// - Iterate empty static `Bytes` and check length zero and the size hint `(0, Some(0))`.
#[test]
fn empty_iter_len() {
    let buf = Bytes::from_static(b"");
    let iter = buf.iter();

    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
}
