//! ASCII character classes from RFC 3986.

use std::borrow::Cow;

use crate::error::{ErrorKind, InvalidUrl};

#[derive(Copy, Clone)]
pub(crate) struct Set(u128);

impl Set {
    pub(crate) const fn new(chars: &[u8]) -> Set {
        let mut bits = 0u128;
        let mut i = 0;
        while i < chars.len() {
            bits |= 1 << chars[i];
            i += 1;
        }
        Set(bits)
    }

    pub(crate) const fn union(self, other: Set) -> Set {
        Set(self.0 | other.0)
    }

    pub(crate) const fn contains(self, b: u8) -> bool {
        b < 128 && self.0 & (1 << b) != 0
    }
}

const fn alnum() -> Set {
    let mut bits = 0u128;
    let mut b = 0u8;
    while b < 128 {
        if b.is_ascii_alphanumeric() {
            bits |= 1 << b;
        }
        b += 1;
    }
    Set(bits)
}

pub(crate) const NONE: Set = Set(0);
pub(crate) const UNRESERVED: Set = alnum().union(Set::new(b"-._~"));
pub(crate) const SUB_DELIMS: Set = Set::new(b"!$&'()*+,;=");
pub(crate) const SUB_DELIMS_WITHOUT_QS: Set = Set::new(b"!$'()*,");
pub(crate) const QS: Set = Set::new(b"+&=;");
pub(crate) const ALLOWED: Set = UNRESERVED.union(SUB_DELIMS_WITHOUT_QS);

pub(crate) const SCHEME: Set = alnum().union(Set::new(b"+-."));
pub(crate) const REG_NAME: Set = UNRESERVED.union(SUB_DELIMS);
pub(crate) const USERINFO: Set = REG_NAME.union(Set::new(b":"));
pub(crate) const PCHAR: Set = REG_NAME.union(Set::new(b":@"));
pub(crate) const PATH: Set = PCHAR.union(Set::new(b"/"));
pub(crate) const QUERY: Set = PCHAR.union(Set::new(b"/?"));

pub(crate) const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

pub(crate) const fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decodes `%XX` at `bytes[i]`, if it is a valid escape.
pub(crate) fn pct_at(bytes: &[u8], i: usize) -> Option<u8> {
    if bytes.get(i) != Some(&b'%') {
        return None;
    }
    let hi = hex_value(*bytes.get(i + 1)?)?;
    let lo = hex_value(*bytes.get(i + 2)?)?;
    Some((hi << 4) | lo)
}

pub(crate) fn push_pct(out: &mut String, b: u8) {
    out.push('%');
    out.push(HEX_UPPER[usize::from(b >> 4)] as char);
    out.push(HEX_UPPER[usize::from(b & 15)] as char);
}

pub(crate) fn char_at(s: &str, i: usize) -> char {
    s[i..].chars().next().unwrap_or('\0')
}

/// Strictly checks that `s` consists of `set` characters and valid `%XX` escapes.
pub(crate) fn check(s: &str, set: Set) -> Result<(), InvalidUrl> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if set.contains(b) {
            i += 1;
        } else if b == b'%' {
            if pct_at(bytes, i).is_none() {
                return Err(InvalidUrl::at(ErrorKind::InvalidPercentEncoding, i));
            }
            i += 3;
        } else {
            return Err(InvalidUrl::at(ErrorKind::InvalidChar(char_at(s, i)), i));
        }
    }
    Ok(())
}

/// Returns the length of the scheme if `s` starts with `scheme ":"`.
pub(crate) fn scheme_len(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if !bytes.first()?.is_ascii_alphabetic() {
        return None;
    }
    let len = bytes.iter().position(|b| !SCHEME.contains(*b))?;
    (bytes[len] == b':').then_some(len)
}

/// ASCII-lowercases `s`, borrowing if it has no uppercase letters.
pub(crate) fn lowercase(s: Cow<'_, str>) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_ascii_lowercase())
    } else {
        s
    }
}

/// Splits `s` at the first `c`.
pub(crate) fn split_at_char(s: &str, c: char) -> (&str, Option<&str>) {
    match s.split_once(c) {
        Some((head, tail)) => (head, Some(tail)),
        None => (s, None),
    }
}
