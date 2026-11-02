#![deny(warnings, rust_2018_idioms)]

use serde_test::{Token, assert_tokens};

/// STATEMENT: Empty `Bytes` serialize and deserialize as an empty bytes token.
///
/// HOW:
/// - Pass `Bytes::new()` and `Token::Bytes(b"")` to `assert_tokens` to check both directions.
#[test]
fn test_ser_de_empty() {
    let b = ntex_bytes::Bytes::new();
    assert_tokens(&b, &[Token::Bytes(b"")]);
}

/// STATEMENT: Nonempty `Bytes` serialize and deserialize without changing their byte payload.
///
/// HOW:
/// - Construct `Bytes` containing `bytes` and use `assert_tokens` with the matching bytes token.
#[test]
fn test_ser_de() {
    let b = ntex_bytes::Bytes::from(&b"bytes"[..]);
    assert_tokens(&b, &[Token::Bytes(b"bytes")]);
}
