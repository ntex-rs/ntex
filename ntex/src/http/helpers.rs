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
