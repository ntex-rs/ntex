//! Percent-encoding inspired by Python's yarl.
//!
//! * [`quote`] encodes a literal value: every character that isn't allowed in the
//!   component, including `%`, is percent-encoded.
//! * [`requote`] normalizes an already-encoded value: valid `%XX` escapes are kept
//!   (uppercased, or decoded when they encode an unreserved character `A-Z a-z
//!   0-9 - . _ ~`), any other
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

use crate::chars::{ALLOWED, HEX_UPPER, NONE, QS, Set, UNRESERVED, pct_at, push_pct};

/// The URL component a value belongs to; it selects the characters that are
/// left unencoded.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Component {
    /// User name or password. `:` and `@` are encoded.
    UserInfo,
    /// Path. `/` is kept.
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

struct Quoter {
    safe: Set,
    qs: bool,
}

static USERINFO: Quoter = Quoter {
    safe: ALLOWED.union(QS),
    qs: false,
};
static PATH: Quoter = Quoter {
    safe: ALLOWED.union(QS).union(Set::new(b"@:/+")),
    qs: false,
};
static QUERY: Quoter = Quoter {
    safe: ALLOWED.union(Set::new(b"?/:@=+&;")),
    qs: true,
};
// a literal `+` in a query would be decoded as space
static QUERY_LITERAL: Quoter = Quoter {
    safe: QUERY.safe.without(b'+'),
    qs: true,
};
static QUERY_PART: Quoter = Quoter {
    safe: ALLOWED.union(Set::new(b"?/:@")),
    qs: true,
};
static FRAGMENT: Quoter = Quoter {
    safe: ALLOWED.union(QS).union(Set::new(b"?/:@")),
    qs: false,
};
static OPAQUE: Quoter = Quoter {
    safe: UNRESERVED,
    qs: false,
};

const PATH_SAFE: Set = Set::new(b"/%");

impl Component {
    fn quoter(self, requote: bool) -> &'static Quoter {
        match self {
            Component::UserInfo => &USERINFO,
            Component::Path => &PATH,
            Component::Query if requote => &QUERY,
            Component::Query => &QUERY_LITERAL,
            Component::QueryPart => &QUERY_PART,
            Component::Fragment => &FRAGMENT,
            Component::Opaque => &OPAQUE,
        }
    }
}

/// Percent-encodes a literal value.
pub fn quote(src: &str, component: Component) -> Cow<'_, str> {
    quote_with::<false>(src, component)
}

/// Normalizes the percent-encoding of an already-encoded value.
pub fn requote(src: &str, component: Component) -> Cow<'_, str> {
    quote_with::<true>(src, component)
}

/// Decodes a percent-encoded value.
///
/// For [`Component::QueryPart`] `+` is decoded as space. For [`Component::Query`]
/// `+` is decoded as space too, but escapes of `&`, `=`, `+` and `;` are kept, so
/// the result can still be split into pairs.
pub fn unquote(src: &str, component: Component) -> Cow<'_, str> {
    match component {
        Component::Query => unquote_with::<true>(src, &QS),
        Component::QueryPart => unquote_with::<true>(src, &NONE),
        _ => unquote_with::<false>(src, &NONE),
    }
}

fn quote_with<const REQUOTE: bool>(src: &str, component: Component) -> Cow<'_, str> {
    let q = component.quoter(REQUOTE);
    let bytes = src.as_bytes();
    let is_safe = |b: u8| q.safe.contains(b);

    // fast path, find the first byte that has to change
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if is_safe(b) {
            i += 1;
            continue;
        }
        if REQUOTE
            && let Some(v) = pct_at(bytes, i)
            && !bytes[i + 1].is_ascii_lowercase()
            && !bytes[i + 2].is_ascii_lowercase()
            && !UNRESERVED.contains(v)
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
    if bytes.len() - i < 32 {
        quote_into::<REQUOTE, false>(&mut out, src, i, q);
    } else {
        quote_into::<REQUOTE, true>(&mut out, src, i, q);
    }
    Cow::Owned(out)
}

#[inline(always)]
fn quote_into<const REQUOTE: bool, const BULK: bool>(
    out: &mut String,
    src: &str,
    mut i: usize,
    q: &Quoter,
) {
    let bytes = src.as_bytes();
    while i < bytes.len() {
        let b = bytes[i];
        if q.safe.contains(b) {
            if BULK {
                let mut end = i + 1;
                while end < bytes.len() && q.safe.contains(bytes[end]) {
                    end += 1;
                }
                if end - i >= 8 {
                    out.push_str(&src[i..end]);
                    i = end;
                    continue;
                }
                return quote_into::<REQUOTE, false>(out, src, i, q);
            }
            out.push(b as char);
            i += 1;
        } else if REQUOTE && let Some(v) = pct_at(bytes, i) {
            if !UNRESERVED.contains(v) {
                push_pct(out, v);
            } else {
                out.push(v as char);
            }
            i += 3;
        } else if q.qs && b == b' ' {
            out.push('+');
            i += 1;
        } else {
            push_pct(out, b);
            i += 1;
        }
    }
}

/// Decodes `src`, keeping escapes of `ignore` characters encoded.
pub(crate) fn unquote_with<'a, const PLUS: bool>(src: &'a str, ignore: &Set) -> Cow<'a, str> {
    let bytes = src.as_bytes();
    let Some(start) = bytes
        .iter()
        .position(|b| *b == b'%' || (PLUS && *b == b'+'))
    else {
        return Cow::Borrowed(src);
    };

    // fast path, decode everything and validate once
    let mut buf = Vec::with_capacity(bytes.len());
    buf.extend_from_slice(&bytes[..start]);
    unquote_into::<PLUS, true>(&mut buf, bytes, start, ignore);
    if simdutf8::basic::from_utf8(&buf[start..]).is_ok() {
        // SAFETY: the unchanged prefix is valid UTF-8 ending at an ASCII escape
        // boundary, and the decoded suffix is validated above.
        return Cow::Owned(unsafe { String::from_utf8_unchecked(buf) });
    }
    let mut out = String::with_capacity(bytes.len());
    out.push_str(&src[..start]);
    // Literal UTF-8 cannot complete an escaped partial sequence: it never starts
    // with a continuation byte. Repairing the decoded buffer preserves run boundaries.
    for chunk in buf[start..].utf8_chunks() {
        out.push_str(chunk.valid());
        for &b in chunk.invalid() {
            push_pct(&mut out, b);
        }
    }
    Cow::Owned(out)
}

// Specialize short-run tails so dense escapes do not pay a per-byte bulk-copy branch.
#[inline(always)]
fn unquote_into<const PLUS: bool, const BULK: bool>(
    buf: &mut Vec<u8>,
    bytes: &[u8],
    mut i: usize,
    ignore: &Set,
) {
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(v) = pct_at(bytes, i) {
            if ignore.contains(v) {
                buf.extend_from_slice(&[
                    b'%',
                    HEX_UPPER[usize::from(v >> 4)],
                    HEX_UPPER[usize::from(v & 15)],
                ]);
            } else {
                buf.push(v);
            }
            i += 3;
        } else {
            if BULK && b != b'%' && !(PLUS && b == b'+') {
                let mut end = i + 1;
                while end < bytes.len() && bytes[end] != b'%' && !(PLUS && bytes[end] == b'+') {
                    end += 1;
                }
                if end - i >= 8 {
                    buf.extend_from_slice(&bytes[i..end]);
                    i = end;
                    continue;
                }
                return unquote_into::<PLUS, false>(buf, bytes, i, ignore);
            }
            buf.push(if PLUS && b == b'+' { b' ' } else { b });
            i += 1;
        }
    }
}

/// Decodes a path, keeping `%2F` and `%25` encoded.
pub(crate) fn unquote_path_safe(src: &str) -> Cow<'_, str> {
    unquote_with::<false>(src, &PATH_SAFE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: Specialized quote modes preserve component rules across short and long safe runs.
    ///
    /// HOW:
    /// - Surround Unicode, spaces, delimiters and percent escapes with varying ASCII spans.
    /// - Check literal and normalized output, and borrowing for unchanged canonical inputs.
    #[test]
    fn quoting_modes_and_spans() {
        for len in [0, 1, 7, 8, 15, 16, 31, 32, 63, 64, 127] {
            let prefix = "a".repeat(len);
            let suffix = "z".repeat(len);
            for (component, input, quoted, requoted) in [
                (
                    Component::Path,
                    "a b/\u{fc}",
                    "a%20b/%C3%BC",
                    "a%20b/%C3%BC",
                ),
                (
                    Component::Path,
                    "%7e%41%2f%2b",
                    "%257e%2541%252f%252b",
                    "~A%2F%2B",
                ),
                (Component::Path, "%zz%4", "%25zz%254", "%25zz%254"),
                (Component::Query, "a+b=c d", "a%2Bb=c+d", "a+b=c+d"),
                (
                    Component::QueryPart,
                    "a+b=c&d;e",
                    "a%2Bb%3Dc%26d%3Be",
                    "a%2Bb%3Dc%26d%3Be",
                ),
                (Component::UserInfo, "u:p@x", "u%3Ap%40x", "u%3Ap%40x"),
                (
                    Component::Fragment,
                    "x#\u{1f642}",
                    "x%23%F0%9F%99%82",
                    "x%23%F0%9F%99%82",
                ),
                (Component::Opaque, "=?#\0", "%3D%3F%23%00", "%3D%3F%23%00"),
            ] {
                let src = format!("{prefix}{input}{suffix}");
                assert_eq!(quote(&src, component), format!("{prefix}{quoted}{suffix}"));
                assert_eq!(
                    requote(&src, component),
                    format!("{prefix}{requoted}{suffix}")
                );
            }
            let src = format!("{prefix}%2F%20{suffix}");
            assert!(matches!(requote(&src, Component::Path), Cow::Borrowed(_)));
            let src = format!("{prefix}-{suffix}");
            assert!(matches!(quote(&src, Component::Opaque), Cow::Borrowed(_)));
            let src = format!(" {prefix}\u{fc}{suffix} ");
            assert_eq!(
                quote(&src, Component::Path),
                format!("%20{prefix}%C3%BC{suffix}%20")
            );
        }
    }

    /// STATEMENT: Specialized decoding handles every escaped byte without changing literal UTF-8.
    ///
    /// HOW:
    /// - Surround each lowercase byte escape with literal Unicode and a literal plus.
    /// - Check path, query and query-part plus handling and protected-escape normalization.
    /// - Check the safe-path decoder preserves slash and percent escapes.
    #[test]
    fn unquote_bytes_and_modes() {
        for byte in 0..=255u8 {
            let src = format!("\u{e9}+%{byte:02x}\u{1f642}");
            let encoded = format!("%{byte:02X}");
            let decoded = if byte.is_ascii() {
                char::from(byte).to_string()
            } else {
                encoded.clone()
            };
            for component in [Component::Path, Component::Query, Component::QueryPart] {
                let plus = if component == Component::Path { "+" } else { " " };
                let value = if component == Component::Query && b"&=+;".contains(&byte) {
                    &encoded
                } else {
                    &decoded
                };
                assert_eq!(
                    unquote(&src, component),
                    format!("\u{e9}{plus}{value}\u{1f642}")
                );
            }
            let value = if b"/%".contains(&byte) { &encoded } else { &decoded };
            assert_eq!(unquote_path_safe(&src), format!("\u{e9}+{value}\u{1f642}"));
        }
    }

    /// STATEMENT: Repairing decoded bytes preserves escaped-run boundaries and literal percent text.
    ///
    /// HOW:
    /// - Mix valid escaped Unicode, invalid/truncated UTF-8, raw Unicode and malformed escapes.
    /// - Check invalid bytes are re-encoded once, without reinterpreting decoded percent signs.
    /// - Exercise long valid prefixes and ignored query/path escapes alongside invalid bytes.
    #[test]
    fn unquote_invalid_runs() {
        for len in [0, 1, 15, 16, 63, 64, 127] {
            let prefix = "a".repeat(len);
            for (input, expected) in [
                ("%ff%C3%BCx%C3", "%FF\u{fc}x%C3"),
                ("%E2%28%A1", "%E2(%A1"),
                ("%C0%AF", "%C0%AF"),
                ("%ED%A0%80", "%ED%A0%80"),
                ("%F4%90%80%80", "%F4%90%80%80"),
                ("%F0%9F%92", "%F0%9F%92"),
                ("%E2%82%AC%E2%82", "\u{20ac}%E2%82"),
                ("%e2\u{20ac}%82%ac", "%E2\u{20ac}%82%AC"),
                ("%ff%25ff", "%FF%ff"),
                ("\u{e9}%ff%25ff", "\u{e9}%FF%ff"),
                ("%ff%25C3%25BC", "%FF%C3%BC"),
                ("%ff%zz%4", "%FF%zz%4"),
                (
                    "%ff\u{e9}\u{20ac}\u{1f600}%20",
                    "%FF\u{e9}\u{20ac}\u{1f600} ",
                ),
            ] {
                let src = format!("{prefix}{input}");
                assert_eq!(
                    unquote(&src, Component::Path),
                    format!("{prefix}{expected}")
                );
            }
            let src = format!("{prefix}%ff+%26%3d%2b%3b");
            assert_eq!(
                unquote(&src, Component::Query),
                format!("{prefix}%FF %26%3D%2B%3B")
            );
            assert_eq!(
                unquote(&src, Component::QueryPart),
                format!("{prefix}%FF &=+;")
            );
            let src = format!("{prefix}%ff%2f%25%C3%BC");
            assert_eq!(unquote_path_safe(&src), format!("{prefix}%FF%2F%25\u{fc}"));
        }
    }

    /// STATEMENT: Scalar span copying preserves malformed escapes and borrows unchanged inputs.
    ///
    /// HOW:
    /// - Separate escapes, plus signs and malformed percent text with safe runs of varying lengths.
    /// - Check path/query decoding and borrowing when no transformation is needed.
    #[test]
    fn unquote_spans() {
        for len in [0, 1, 7, 8, 15, 16, 31, 32, 63, 64, 127] {
            let span = "x".repeat(len);
            let src = format!("%41{span}+{span}%20{span}%zz{span}%");
            assert_eq!(
                unquote(&src, Component::Path),
                format!("A{span}+{span} {span}%zz{span}%")
            );
            assert_eq!(
                unquote(&src, Component::QueryPart),
                format!("A{span} {span} {span}%zz{span}%")
            );
            let src = format!("\u{fc}{span}");
            assert!(matches!(unquote(&src, Component::Path), Cow::Borrowed(_)));
            assert!(matches!(
                unquote(&src, Component::QueryPart),
                Cow::Borrowed(_)
            ));
        }
    }

    /// STATEMENT: Literal quoting uses the safe set and space rules of each component.
    ///
    /// HOW:
    /// - Check representative delimiters, Unicode and percent signs in every component.
    /// - Confirm already-safe opaque values are borrowed.
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

    /// STATEMENT: Requoting normalizes escapes without decoding protected component delimiters.
    ///
    /// HOW:
    /// - Check canonical borrowing, lowercase escapes, malformed escapes and query spaces.
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

    /// STATEMENT: Unquoting preserves invalid UTF-8 and component-specific protected escapes.
    ///
    /// HOW:
    /// - Decode Unicode and plus signs, checking path, query and safe-path results.
    /// - Confirm unchanged text is borrowed and malformed escapes remain literal.
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
