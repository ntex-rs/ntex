//! Various http headers

pub use http::header::{HeaderName, HeaderValue, InvalidHeaderValue};

pub(crate) mod map;

#[doc(hidden)]
pub use self::map::GetAll;
pub use self::map::HeaderMap;

/// Represents supported types of content encodings
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum ContentEncoding {
    /// Automatically select encoding based on encoding negotiation
    Auto,
    /// A format using the Brotli algorithm
    Br,
    /// A format using the zlib structure with deflate algorithm
    Deflate,
    /// Gzip algorithm
    Gzip,
    /// Indicates the identity function (i.e. no compression, nor modification)
    Identity,
}

impl ContentEncoding {
    #[inline]
    /// Is the content compressed?
    pub fn is_compressed(self) -> bool {
        !matches!(self, ContentEncoding::Identity | ContentEncoding::Auto)
    }

    #[inline]
    /// Convert content encoding to string
    pub fn as_str(self) -> &'static str {
        match self {
            ContentEncoding::Br => "br",
            ContentEncoding::Gzip => "gzip",
            ContentEncoding::Deflate => "deflate",
            ContentEncoding::Identity | ContentEncoding::Auto => "identity",
        }
    }

    #[inline]
    /// default quality value
    pub fn quality(self) -> f64 {
        match self {
            ContentEncoding::Br => 1.1,
            ContentEncoding::Gzip => 1.0,
            ContentEncoding::Deflate => 0.9,
            ContentEncoding::Identity | ContentEncoding::Auto => 0.1,
        }
    }
}

impl<'a> From<&'a str> for ContentEncoding {
    fn from(s: &'a str) -> ContentEncoding {
        let s = s.trim();

        if s.eq_ignore_ascii_case("br") {
            ContentEncoding::Br
        } else if s.eq_ignore_ascii_case("gzip") {
            ContentEncoding::Gzip
        } else if s.eq_ignore_ascii_case("deflate") {
            ContentEncoding::Deflate
        } else {
            ContentEncoding::Identity
        }
    }
}

/// Convert http::HeaderMap to a HeaderMap
impl From<http::HeaderMap> for HeaderMap {
    fn from(map: http::HeaderMap) -> HeaderMap {
        let mut new_map = HeaderMap::with_capacity(map.capacity());
        for (h, v) in map.iter() {
            new_map.append(h.clone(), v.clone());
        }
        new_map
    }
}

pub use http::header::{
    ACCEPT, ACCEPT_CHARSET, ACCEPT_ENCODING, ACCEPT_LANGUAGE, ACCEPT_RANGES,
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS,
    ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, AGE, ALLOW, ALT_SVC,
    AUTHORIZATION, CACHE_CONTROL, CONNECTION, CONTENT_DISPOSITION, CONTENT_ENCODING,
    CONTENT_LANGUAGE, CONTENT_LENGTH, CONTENT_LOCATION, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY_REPORT_ONLY, CONTENT_TYPE, COOKIE,
    DATE, DNT, ETAG, EXPECT, EXPIRES, FORWARDED, FROM, HOST, IF_MATCH,
    IF_MODIFIED_SINCE, IF_NONE_MATCH, IF_RANGE, IF_UNMODIFIED_SINCE, LAST_MODIFIED,
    LINK, LOCATION, MAX_FORWARDS, ORIGIN, PRAGMA, PROXY_AUTHENTICATE,
    PROXY_AUTHORIZATION, PUBLIC_KEY_PINS, PUBLIC_KEY_PINS_REPORT_ONLY, RANGE, REFERER,
    REFERRER_POLICY, REFRESH, RETRY_AFTER, SEC_WEBSOCKET_ACCEPT,
    SEC_WEBSOCKET_EXTENSIONS, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_PROTOCOL,
    SEC_WEBSOCKET_VERSION, SERVER, SET_COOKIE, STRICT_TRANSPORT_SECURITY, TE, TRAILER,
    TRANSFER_ENCODING, UPGRADE, UPGRADE_INSECURE_REQUESTS, USER_AGENT, VARY, VIA,
    WARNING, WWW_AUTHENTICATE, X_CONTENT_TYPE_OPTIONS, X_DNS_PREFETCH_CONTROL,
    X_FRAME_OPTIONS, X_XSS_PROTECTION,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding() {
        assert!(ContentEncoding::Br.is_compressed());
        assert!(!ContentEncoding::Identity.is_compressed());
        assert!(!ContentEncoding::Auto.is_compressed());
        assert_eq!(format!("{:?}", ContentEncoding::Identity), "Identity");
    }
}
