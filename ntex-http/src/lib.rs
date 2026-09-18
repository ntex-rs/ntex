//! HTTP protocol primitives for ntex.
//!
//! This crate provides [`HeaderMap`] and [`HeaderValue`] implementations,
//! message-body abstractions in [`body`], common HTTP errors, and re-exports of
//! standard method, status, URI, version, and header-name types.
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value
)]

pub mod body;
pub mod error;
pub mod header;
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

/// Converts an `http::HeaderMap` into an ntex [`HeaderMap`].
impl From<http::HeaderMap> for HeaderMap {
    fn from(map: http::HeaderMap) -> HeaderMap {
        let mut new_map = HeaderMap::with_capacity(map.capacity());
        for (h, v) in &map {
            new_map.append(h.clone(), HeaderValue::from(v));
        }
        new_map
    }
}

#[doc(hidden)]
pub mod compat {
    pub use http::header::InvalidHeaderName;
    pub use http::header::InvalidHeaderValue;
    pub use http::method::InvalidMethod;
    pub use http::status::InvalidStatusCode;
    pub use http::uri::InvalidUri;
}
