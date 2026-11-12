//! ASCII character classes from RFC 3986.

use std::borrow::Cow;

use crate::error::{ErrorKind, InvalidUrl};

/// A byte lookup table, faster than a bit set in scanning loops.
#[derive(Copy, Clone)]
pub(crate) struct Set([bool; 256]);

impl Set {
    pub(crate) const fn new(chars: &[u8]) -> Set {
        let mut table = [false; 256];
        let mut i = 0;
        while i < chars.len() {
            table[chars[i] as usize] = true;
            i += 1;
        }
        Set(table)
    }

    pub(crate) const fn union(self, other: Set) -> Set {
        let mut table = self.0;
        let mut i = 0;
        while i < 256 {
            table[i] |= other.0[i];
            i += 1;
        }
        Set(table)
    }

    pub(crate) const fn without(self, b: u8) -> Set {
        let mut table = self.0;
        table[b as usize] = false;
        Set(table)
    }

    #[inline]
    pub(crate) const fn contains(&self, b: u8) -> bool {
        self.0[b as usize]
    }
}

const fn alnum() -> Set {
    let mut table = [false; 256];
    let mut b = 0u8;
    while b < 128 {
        table[b as usize] = b.is_ascii_alphanumeric();
        b += 1;
    }
    Set(table)
}

pub(crate) const NONE: Set = Set([false; 256]);
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

#[inline]
pub(crate) fn push_pct(out: &mut String, b: u8) {
    let encoded = [
        b'%',
        HEX_UPPER[usize::from(b >> 4)],
        HEX_UPPER[usize::from(b & 15)],
    ];
    // SAFETY: '%' and the hexadecimal digits are ASCII.
    out.push_str(unsafe { std::str::from_utf8_unchecked(&encoded) });
}

pub(crate) fn char_at(s: &str, i: usize) -> char {
    s[i..].chars().next().unwrap_or('\0')
}

/// Strictly checks that `s` consists of `set` characters and valid `%XX` escapes.
pub(crate) fn check(s: &str, set: &Set) -> Result<(), InvalidUrl> {
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

/// Splits `s` at the first ASCII `c`.
pub(crate) fn split_at_char(s: &str, c: u8) -> (&str, Option<&str>) {
    debug_assert!(c.is_ascii());
    match find_byte(s.as_bytes(), c) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    }
}

// Word-at-a-time byte search: XOR a word with `c` repeated in every byte,
// then look for a zero byte. Words are loaded little-endian, so byte `i` of
// the slice is bits `8 * i..8 * i + 8` on every target. Faster than `memchr`
// for the short strings found in URLs.
const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

#[inline(always)]
fn word(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

/// Sets the high bit of the lowest zero byte of `w`, and possibly of bytes
/// above it; zero if `w` has no zero byte.
#[inline(always)]
fn first_zero(w: u64) -> u64 {
    w.wrapping_sub(LO) & !w & HI
}

/// Sets the high bit of exactly the zero bytes of `w`.
#[inline(always)]
fn zero_bytes(w: u64) -> u64 {
    const M7: u64 = !HI;
    !((w & M7).wrapping_add(M7) | w | M7)
}

/// Returns the position of the first `c` in `b`.
pub(crate) fn find_byte(b: &[u8], c: u8) -> Option<usize> {
    let pat = LO * u64::from(c);
    let mut off = 0;
    for ch in b.as_chunks::<16>().0 {
        if first_zero(word(ch) ^ pat) | first_zero(word(&ch[8..]) ^ pat) != 0 {
            break;
        }
        off += 16;
    }
    let (words, tail) = b[off..].as_chunks::<8>();
    for w in words {
        let z = first_zero(word(w) ^ pat);
        if z != 0 {
            return Some(off + (z.trailing_zeros() / 8) as usize);
        }
        off += 8;
    }
    let pos = tail.iter().position(|&x| x == c)?;
    Some(off + pos)
}

/// Returns the position of the last `c` in `b`.
pub(crate) fn rfind_byte(b: &[u8], c: u8) -> Option<usize> {
    if b.len() < 8 {
        return b.iter().rposition(|&x| x == c);
    }
    let last = |z: u64| (63 - z.leading_zeros()) as usize / 8;
    let pat = LO * u64::from(c);
    let mut end = b.len();
    while end >= 8 {
        let z = zero_bytes(word(&b[end - 8..]) ^ pat);
        if z != 0 {
            return Some(end - 8 + last(z));
        }
        end -= 8;
    }
    // the first `end` bytes, read as the low bytes of the first word
    let z = zero_bytes(word(b) ^ pat) & ((1 << (end * 8)) - 1);
    (z != 0).then(|| last(z))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(set: &Set) -> Vec<u8> {
        (0..=255).filter(|b| set.contains(*b)).collect()
    }

    #[test]
    fn sets() {
        // build at runtime to check the const constructors
        let set = Set::new(b"abc").union(Set::new(b"xy")).without(b'b');
        assert_eq!(members(&set), b"acxy");
        assert_eq!(members(&alnum()).len(), 62);
        assert_eq!(members(&NONE), b"");

        let mut expected: Vec<u8> = (0..=255u8)
            .filter(|b| b.is_ascii_alphanumeric() || b"-._~".contains(b))
            .collect();
        assert_eq!(members(&UNRESERVED), expected);
        expected.extend_from_slice(b"!$&'()*+,;=:@/?");
        expected.sort_unstable();
        assert_eq!(members(&QUERY), expected);
        assert!((0x80..=0xff).all(|b| !QUERY.contains(b)));
    }

    #[test]
    fn checks() {
        assert!(check("a%2Fb", &PATH).is_ok());
        let err = check("ab%2", &PATH).unwrap_err();
        assert_eq!(
            (err.kind(), err.position()),
            (ErrorKind::InvalidPercentEncoding, Some(2))
        );
        let err = check("a%zz", &PATH).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidPercentEncoding);
        let err = check("aé", &PATH).unwrap_err();
        assert_eq!(
            (err.kind(), err.position()),
            (ErrorKind::InvalidChar('é'), Some(1))
        );
        assert_eq!(pct_at(b"%4a", 0), Some(0x4a));
        assert_eq!(pct_at(b"a%4", 1), None);
        assert_eq!(char_at("aé", 1), 'é');
        assert_eq!(char_at("a", 1), '\0');
    }

    /// STATEMENT: `find_byte` and `rfind_byte` return the same positions as a
    /// byte-by-byte scan, with no false matches on bytes near the needle.
    ///
    /// HOW:
    /// - Fill buffers of 0 to 47 bytes with a cycle of `0x3e`, `0xbf`, `0x80`,
    ///   `0xff`, and `0x00`; `0x3e` and `0xbf` differ from `?` by one bit.
    /// - Put `?` at no position, one position, or two positions.
    /// - Compare both functions with `position` and `rposition`.
    #[test]
    fn byte_search() {
        let check = |b: &[u8]| {
            assert_eq!(
                find_byte(b, b'?'),
                b.iter().position(|&x| x == b'?'),
                "{b:?}"
            );
            assert_eq!(
                rfind_byte(b, b'?'),
                b.iter().rposition(|&x| x == b'?'),
                "{b:?}"
            );
        };
        for len in 0..48 {
            let base: Vec<u8> = (0..len)
                .map(|i| [0x3e, 0xbf, 0x80, 0xff, 0][i % 5])
                .collect();
            check(&base);
            for i in 0..len {
                for j in i..len {
                    let mut b = base.clone();
                    b[i] = b'?';
                    b[j] = b'?';
                    check(&b);
                }
            }
        }
    }
}
