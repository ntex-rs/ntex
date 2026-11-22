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

/// How request targets are processed while decoding.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum TargetPolicy {
    /// Targets are passed on as received.
    Raw,
    /// Malformed targets are rejected, valid ones are passed on unchanged.
    Validate,
    /// Malformed targets are rejected, valid ones are normalized.
    Normalize,
}

/// Validates and optionally normalizes the HTTP target according to `policy`.
pub(crate) fn process_request_target(
    policy: TargetPolicy,
    method: &super::Method,
    version: super::Version,
    uri: &mut urly::Url,
) -> Result<(), super::error::DecodeError> {
    // RFC 9112 §3.2.4, RFC 9113 §8.3.1: asterisk-form is OPTIONS only,
    // enforced regardless of the policy
    if uri.path() == "*" && method != super::Method::OPTIONS {
        return Err(super::error::DecodeError::InvalidInput(
            "asterisk-form request-target is only valid for OPTIONS",
        ));
    }
    if policy == TargetPolicy::Raw {
        return Ok(());
    }
    if !is_valid_target(method, version, uri) {
        return Err(super::error::DecodeError::InvalidInput(
            "invalid request-target",
        ));
    }
    if policy == TargetPolicy::Validate {
        return Ok(());
    }
    if version == super::Version::HTTP_2 && uri.path() == "*" {
        *uri = urly::Url::from_static("*");
    } else {
        uri.normalize()?;
    }
    Ok(())
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

    /// STATEMENT: Body extraction copies only when retaining the buffer would waste excessive capacity.
    ///
    /// HOW:
    /// - Extract nearly-full and half-full buffers and compare bytes, pointer reuse, and emptied source buffers.
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
}
