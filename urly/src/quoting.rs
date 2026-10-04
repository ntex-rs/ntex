//! Percent-encoding with the same rules as Python's yarl.
//!
//! * [`quote`] encodes a literal value: every character that isn't allowed in the
//!   component, including `%`, is percent-encoded.
//! * [`requote`] normalizes an already-encoded value: valid `%XX` escapes are kept
//!   (uppercased, or decoded when they encode an unreserved character), any other
//!   disallowed character is percent-encoded.
//! * [`unquote`] decodes a value. Escapes that don't form valid UTF-8 are kept as is.
//!
//! All functions borrow the input if it doesn't need to change.
//!
//! ```
//! use urly::quoting::{Component, quote, requote, unquote};
//!
//! assert_eq!(quote("a b/100%", Component::Path), "a%20b/100%25");
//! assert_eq!(requote("a b/100%25%7e", Component::Path), "a%20b/100%25~");
//! assert_eq!(quote("a b&c=d", Component::QueryPart), "a+b%26c%3Dd");
//! assert_eq!(unquote("a+b%26c", Component::QueryPart), "a b&c");
//! assert_eq!(quote("a=b; c", Component::Opaque), "a%3Db%3B%20c");
//! ```
use std::borrow::Cow;

use crate::chars::{self, ALLOWED, NONE, QS, Set, UNRESERVED, pct_at, push_pct};

/// The URL component a value belongs to; it selects the characters that are
/// left unencoded.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Component {
    /// User name or password. `:` and `@` are encoded.
    UserInfo,
    /// Path. `/` is kept; requoting never decodes `%2F`.
    Path,
    /// Whole query string. `&`, `=`, `+` and `;` are kept; space is encoded as `+`.
    Query,
    /// Query key or value. `&`, `=`, `+` and `;` are encoded; space is encoded as `+`.
    QueryPart,
    /// Fragment.
    Fragment,
    /// A standalone value outside of a URL, such as a cookie name or value.
    /// Only unreserved characters (`A-Z a-z 0-9 - . _ ~`) are kept.
    Opaque,
}

#[derive(Copy, Clone)]
struct Quoter {
    safe: Set,
    protected: Set,
    qs: bool,
}

impl Component {
    const fn quoter(self) -> Quoter {
        match self {
            Component::UserInfo => Quoter {
                safe: ALLOWED.union(QS),
                protected: NONE,
                qs: false,
            },
            Component::Path => Quoter {
                safe: ALLOWED.union(QS).union(Set::new(b"@:/+")),
                protected: Set::new(b"/+"),
                qs: false,
            },
            Component::Query => Quoter {
                safe: ALLOWED.union(Set::new(b"?/:@=+&;")),
                protected: QS,
                qs: true,
            },
            Component::QueryPart => Quoter {
                safe: ALLOWED.union(Set::new(b"?/:@")),
                protected: NONE,
                qs: true,
            },
            Component::Fragment => Quoter {
                safe: ALLOWED.union(QS).union(Set::new(b"?/:@")),
                protected: NONE,
                qs: false,
            },
            Component::Opaque => Quoter {
                safe: UNRESERVED,
                protected: NONE,
                qs: false,
            },
        }
    }
}

/// Percent-encodes a literal value.
pub fn quote(src: &str, component: Component) -> Cow<'_, str> {
    quote_with(src, component, false)
}

/// Normalizes the percent-encoding of an already-encoded value.
pub fn requote(src: &str, component: Component) -> Cow<'_, str> {
    quote_with(src, component, true)
}

/// Decodes a percent-encoded value.
///
/// For [`Component::QueryPart`] `+` is decoded as space. For [`Component::Query`]
/// `+` is decoded as space too, but escapes of `&`, `=`, `+` and `;` are kept, so
/// the result can still be split into pairs.
pub fn unquote(src: &str, component: Component) -> Cow<'_, str> {
    match component {
        Component::Query => unquote_with(src, true, QS),
        Component::QueryPart => unquote_with(src, true, NONE),
        _ => unquote_with(src, false, NONE),
    }
}

fn quote_with(src: &str, component: Component, requote: bool) -> Cow<'_, str> {
    let q = component.quoter();
    let bytes = src.as_bytes();
    // a literal `+` in a query would be decoded as space
    let literal_plus = q.qs && !requote;
    let is_safe = |b: u8| q.safe.contains(b) && !(literal_plus && b == b'+');

    // fast path, find the first byte that has to change
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if is_safe(b) {
            i += 1;
            continue;
        }
        if requote
            && let Some(v) = pct_at(bytes, i)
            && !bytes[i + 1].is_ascii_lowercase()
            && !bytes[i + 2].is_ascii_lowercase()
            && (q.protected.contains(v) || !q.safe.contains(v))
        {
            i += 3;
            continue;
        }
        break;
    }
    if i == bytes.len() {
        return Cow::Borrowed(src);
    }

    let mut out = String::with_capacity(bytes.len() + 16);
    out.push_str(&src[..i]);
    while i < bytes.len() {
        let b = bytes[i];
        if is_safe(b) {
            out.push(b as char);
            i += 1;
        } else if requote && let Some(v) = pct_at(bytes, i) {
            if q.protected.contains(v) || !q.safe.contains(v) {
                push_pct(&mut out, v);
            } else {
                out.push(v as char);
            }
            i += 3;
        } else if q.qs && b == b' ' {
            out.push('+');
            i += 1;
        } else {
            push_pct(&mut out, b);
            i += 1;
        }
    }
    Cow::Owned(out)
}

/// Decodes `src`, keeping escapes of `ignore` characters encoded.
pub(crate) fn unquote_with(src: &str, plus: bool, ignore: Set) -> Cow<'_, str> {
    let bytes = src.as_bytes();
    let Some(start) = bytes
        .iter()
        .position(|b| *b == b'%' || (plus && *b == b'+'))
    else {
        return Cow::Borrowed(src);
    };

    let mut out = String::with_capacity(bytes.len());
    out.push_str(&src[..start]);
    let mut pending = Vec::new();
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(v) = pct_at(bytes, i) {
            if ignore.contains(v) {
                flush(&mut out, &mut pending);
                push_pct(&mut out, v);
            } else {
                pending.push(v);
            }
            i += 3;
            continue;
        }
        flush(&mut out, &mut pending);
        if plus && b == b'+' {
            out.push(' ');
            i += 1;
        } else {
            // copy the whole literal char
            let len = utf8_len(b);
            out.push_str(&src[i..i + len]);
            i += len;
        }
    }
    flush(&mut out, &mut pending);
    Cow::Owned(out)
}

const fn utf8_len(b: u8) -> usize {
    match b {
        0..0x80 => 1,
        0xc0..0xe0 => 2,
        0xe0..0xf0 => 3,
        _ => 4,
    }
}

/// Appends decoded bytes; sequences that are not valid UTF-8 are re-encoded.
fn flush(out: &mut String, pending: &mut Vec<u8>) {
    let mut rest = &pending[..];
    while !rest.is_empty() {
        match simdutf8::compat::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: `rest[..valid]` is valid UTF-8
                out.push_str(unsafe { std::str::from_utf8_unchecked(&rest[..valid]) });
                let invalid = e.error_len().unwrap_or(rest.len() - valid);
                for b in &rest[valid..valid + invalid] {
                    push_pct(out, *b);
                }
                rest = &rest[valid + invalid..];
            }
        }
    }
    pending.clear();
}

/// Decodes a path, keeping `%2F` and `%25` encoded.
pub(crate) fn unquote_path_safe(src: &str) -> Cow<'_, str> {
    unquote_with(src, false, chars::Set::new(b"/%"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_rules() {
        assert_eq!(quote("", Component::Path), "");
        assert_eq!(quote("/a b/ü", Component::Path), "/a%20b/%C3%BC");
        assert_eq!(quote("%41", Component::Path), "%2541");
        assert_eq!(quote("u:p@x", Component::UserInfo), "u%3Ap%40x");
        assert_eq!(
            quote("a+b=c&d;e", Component::QueryPart),
            "a%2Bb%3Dc%26d%3Be"
        );
        assert_eq!(quote("a=b&c d", Component::Query), "a=b&c+d");
        assert_eq!(quote("x#y", Component::Fragment), "x%23y");
        assert_eq!(
            quote("a=b; c,\"%~", Component::Opaque),
            "a%3Db%3B%20c%2C%22%25~"
        );
        assert!(matches!(
            quote("a-b_c.1~", Component::Opaque),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn requote_rules() {
        assert!(matches!(
            requote("/a%2Fb%20c", Component::Path),
            Cow::Borrowed(_)
        ));
        assert_eq!(requote("%7e%41%2f%2b", Component::Path), "~A%2F%2B");
        assert_eq!(requote("100%", Component::Path), "100%25");
        assert_eq!(requote("%zz%4", Component::Path), "%25zz%254");
        assert_eq!(requote("a=%3d&b", Component::Query), "a=%3D&b");
        assert_eq!(requote("a b", Component::Query), "a+b");
        assert_eq!(requote("%C3%BC", Component::Fragment), "%C3%BC");
    }

    #[test]
    fn unquote_rules() {
        assert!(matches!(unquote("abc", Component::Path), Cow::Borrowed(_)));
        assert_eq!(unquote("%C3%BC%20x", Component::Path), "ü x");
        assert_eq!(unquote("a+b", Component::Path), "a+b");
        assert_eq!(unquote("a+b%2B", Component::QueryPart), "a b+");
        assert_eq!(unquote("a+b%26c%3D", Component::Query), "a b%26c%3D");
        assert_eq!(unquote("%FF%C3%BCx%C3", Component::Path), "%FFüx%C3");
        assert_eq!(unquote("100%", Component::Path), "100%");
        assert_eq!(unquote_path_safe("a%2Fb%25c%20"), "a%2Fb%25c ");
        assert_eq!(unquote("ü%20", Component::Fragment), "ü ");
    }
}
