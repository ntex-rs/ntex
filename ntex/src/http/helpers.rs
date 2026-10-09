use serde::Serialize;

use crate::util::{Bytes, BytesMut};

/// Serializes `value` as JSON.
///
/// The JSON is written into the buffer that becomes the body, it is not
/// copied from an intermediate `String`.
pub(crate) fn json_body<T: ?Sized + Serialize>(value: &T) -> serde_json::Result<Bytes> {
    let mut buf = BytesMut::new();
    serde_json::to_writer(&mut buf, value)?;
    Ok(buf.freeze())
}

/// Takes the data of `buf`, copied if more than a fifth of the buffer would
/// be unused.
///
/// A body read into a growing buffer can leave almost half of it unused, and
/// the frozen body keeps all of it alive.
pub(crate) fn take_trimmed(buf: &mut BytesMut) -> Bytes {
    let len = buf.len();
    if buf.capacity() > len + len / 4 {
        let body = Bytes::copy_from_slice(buf);
        buf.clear();
        body
    } else {
        buf.take()
    }
}

/// Checks a request-target before it is parsed and normalized.
///
/// Rejects invalid percent-encoding, a fragment, non-ASCII bytes, characters
/// that are never sent unencoded (`"`, `<`, `>`, `\`, whitespace and controls)
/// and an encoded NUL (`%00`) before the query, see RFC 9112 section 3.2.
/// Brackets, braces, `|`, `^` and a backtick, which browsers send unencoded,
/// are accepted and get percent-encoded by `Url` parsing.
pub(crate) fn is_valid_target(target: &[u8]) -> bool {
    let mut path = true;
    let mut i = 0;
    while let Some(&b) = target.get(i) {
        match b {
            b'%' => {
                let (Some(&hi), Some(&lo)) = (target.get(i + 1), target.get(i + 2)) else {
                    return false;
                };
                if !hi.is_ascii_hexdigit()
                    || !lo.is_ascii_hexdigit()
                    || (path && hi == b'0' && lo == b'0')
                {
                    return false;
                }
                i += 3;
                continue;
            }
            b'?' => path = false,
            b'#' | b'"' | b'<' | b'>' | b'\\' | 0..=b' ' | 0x7f.. => return false,
            _ => (),
        }
        i += 1;
    }
    true
}

/// Appends `name=value` of a cookie to a `Cookie` header value.
///
/// Name and value are percent-encoded, only unreserved characters are kept.
#[cfg(feature = "cookie")]
pub(crate) fn push_cookie(buf: &mut Vec<u8>, name: &str, value: &str) {
    use urly::quoting::{Component, quote};

    if !buf.is_empty() {
        buf.extend_from_slice(b"; ");
    }
    buf.extend_from_slice(quote(name, Component::Opaque).as_bytes());
    buf.push(b'=');
    buf.extend_from_slice(quote(value, Component::Opaque).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimmed() {
        let mut buf = BytesMut::with_capacity(1024);
        buf.extend_from_slice(&[1; 1000]);
        let ptr = buf.as_ptr();
        let body = take_trimmed(&mut buf);
        assert_eq!(body, [1; 1000][..]);
        assert_eq!(body.as_ptr(), ptr);
        assert!(buf.is_empty());

        buf.reserve(1024);
        buf.extend_from_slice(&[2; 512]);
        let ptr = buf.as_ptr();
        let body = take_trimmed(&mut buf);
        assert_eq!(body, [2; 512][..]);
        assert_ne!(body.as_ptr(), ptr);
        assert!(buf.is_empty());
    }

    #[test]
    fn valid_target() {
        for target in [
            "/",
            "*",
            "example.com:443",
            "http://example.com/a%2Fb?c",
            "/a[0]|^`{}?a[]=1&q=a|b%00",
            "/a%2e%2E/%41",
        ] {
            assert!(is_valid_target(target.as_bytes()), "{target}");
        }
        for target in [
            "/a#f",
            "/a?b#f",
            "/a%zz",
            "/a%",
            "/a%4",
            "/a?%g0",
            "/a\"",
            "/a<",
            "/a>",
            "/a\\b",
            "/a b",
            "/a\x7f",
            "/ü",
            "/a%00",
            "http://h/%00",
            "%00",
        ] {
            assert!(!is_valid_target(target.as_bytes()), "{target}");
        }
        assert!(!is_valid_target(b"/a\xff"));
    }

    #[test]
    fn json_body_matches_to_string() {
        let small = vec![1, 2, 3];
        let large: Vec<String> = (0..1000).map(|i| format!("item-{i}")).collect();
        let map: std::collections::BTreeMap<_, _> = (0..50).map(|i| (i.to_string(), i)).collect();

        assert_eq!(
            json_body(&small).unwrap(),
            serde_json::to_string(&small).unwrap()
        );
        assert_eq!(
            json_body(&large).unwrap(),
            serde_json::to_string(&large).unwrap()
        );
        assert_eq!(
            json_body(&map).unwrap(),
            serde_json::to_string(&map).unwrap()
        );
        assert_eq!(json_body("").unwrap(), "\"\"");

        // a small body is stored inline
        assert!(json_body(&small).unwrap().is_inline());
    }

    #[test]
    fn json_body_error() {
        let mut map = std::collections::HashMap::new();
        map.insert(vec![1u8], 1);
        assert!(json_body(&map).is_err());
    }
}
