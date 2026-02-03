//! Http protocol support.
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::needless_pass_by_value,
    clippy::must_use_candidate,
    clippy::return_self_not_must_use,
    clippy::missing_errors_doc,
    clippy::struct_field_names
)]

pub mod body;
pub mod error;
mod map;
mod serde;
mod value;

pub use self::error::Error;
pub use self::map::HeaderMap;
pub use self::value::HeaderValue;

#[doc(hidden)]
pub use self::map::Value;

// re-exports
pub use http::header::HeaderName;
pub use http::uri::{self, Uri};
pub use http::{Method, StatusCode, Version};

/// Convert `http::HeaderMap` to a `HeaderMap`
impl From<http::HeaderMap> for HeaderMap {
    fn from(map: http::HeaderMap) -> HeaderMap {
        let mut new_map = HeaderMap::with_capacity(map.capacity());
        for (h, v) in &map {
            new_map.append(h.clone(), HeaderValue::from(v));
        }
        new_map
    }
}

pub mod header {
    //! Various http headers

    #[doc(hidden)]
    pub use crate::map::{AsName, Either, GetAll, Iter, Value};
    pub use crate::value::{HeaderValue, InvalidHeaderValue, ToStrError};

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
    pub use http::header::{HeaderName, InvalidHeaderName};
}

#[doc(hidden)]
pub mod compat {
    pub use http::header::InvalidHeaderName;
    pub use http::header::InvalidHeaderValue;
    pub use http::method::InvalidMethod;
    pub use http::status::InvalidStatusCode;
    pub use http::uri::InvalidUri;
}
