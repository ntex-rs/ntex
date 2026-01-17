#![deny(warnings, rust_2018_idioms)]

use serde_test::{Token, assert_tokens};

#[test]
fn test_ser_de_empty() {
    let b = ntex_bytes::Bytes::new();
    assert_tokens(&b, &[Token::Bytes(b"")]);
}

#[test]
fn test_ser_de() {
    let b = ntex_bytes::Bytes::from(&b"bytes"[..]);
    assert_tokens(&b, &[Token::Bytes(b"bytes")]);
}
