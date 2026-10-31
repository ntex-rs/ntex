use serde::Serialize;

use crate::util::{Bytes, BytesMut};

/// Checks target syntax and the method/protocol's target form without rewriting it.
pub(crate) fn is_valid_target(
    method: &super::Method,
    version: super::Version,
    uri: &urly::Url,
) -> bool {
    if uri.validate_target().is_err() {
        return false;
    }
    if method == super::Method::CONNECT {
        !uri.is_absolute()
            && uri.authority().is_some()
            && uri.path().is_empty()
            && uri.query().is_none()
    } else if version != super::Version::HTTP_2 && uri.is_absolute() {
        uri.authority().is_some()
    } else {
        (version == super::Version::HTTP_2 || uri.authority().is_none())
            && (uri.path().as_str().starts_with('/')
                || (uri.path() == "*" && uri.query().is_none() && method == super::Method::OPTIONS))
    }
}

/// Validates the HTTP target form before normalizing its components.
pub(crate) fn normalize_request_target(
    method: &super::Method,
    version: super::Version,
    uri: &mut urly::Url,
) -> Result<(), super::error::DecodeError> {
    if !is_valid_target(method, version, uri) {
        return Err(super::error::DecodeError::InvalidInput(
            "invalid request-target",
        ));
    }
    if version == super::Version::HTTP_2 && uri.path() == "*" {
        *uri = urly::Url::from_static("*");
    } else {
        uri.normalize()?;
    }
    Ok(())
}

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
