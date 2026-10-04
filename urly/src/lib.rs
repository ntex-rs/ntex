//! URL manipulation library with an API inspired by Python's
//! [yarl](https://yarl.aio-libs.org/).
//!
//! [`Url`] is an immutable, cheaply cloneable, normalized URL reference stored
//! in a single [`ByteString`](ntex_bytes::ByteString). Accessors return
//! borrowed, percent-encoded components with methods to decode them;
//! modifications return or produce a new normalized URL.
//!
//! ```
//! use urly::Url;
//!
//! let url = Url::from_static("https://example.com/api/v1?page=2");
//! assert_eq!(url.host(), Some("example.com"));
//! assert_eq!(url.port_or_known_default(), Some(443));
//!
//! let users = &url / "users" / "john doe";
//! assert_eq!(users, "https://example.com/api/v1/users/john%20doe");
//! assert_eq!(users.path().segments().last().unwrap(), "john%20doe");
//!
//! let mut url = users.join("../groups?id=1").unwrap();
//! url.extend_query_pairs([("sort", "name asc")]);
//! assert_eq!(url, "https://example.com/api/v1/groups?id=1&sort=name+asc");
//!
//! let url = Url::builder()
//!     .scheme("http")
//!     .host("münchen.de")
//!     .path("/a b")
//!     .query_pair("q", "x&y")
//!     .build()
//!     .unwrap();
//! assert_eq!(url, "http://xn--mnchen-3ya.de/a%20b?q=x%26y");
//! ```
//!
//! # Features
//!
//! * `http` - conversions between [`Url`] and `http::Uri`
//! * `serde` - `Serialize` and `Deserialize` for [`Url`]
#![deny(missing_docs)]

macro_rules! str_type {
    ($name:ident) => {
        impl $name {
            #[allow(dead_code)]
            pub(crate) const fn from_str_unchecked(s: &str) -> &$name {
                // SAFETY: the type is a `repr(transparent)` wrapper around `str`
                unsafe { &*(s as *const str as *const $name) }
            }

            /// Returns the component as a string slice.
            pub const fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'a> TryFrom<&'a str> for &'a $name {
            type Error = $crate::InvalidUrl;

            fn try_from(s: &'a str) -> Result<Self, Self::Error> {
                $name::new(s)
            }
        }

        str_fmt!($name);
    };
}

/// `AsRef<str>`, `Display` and `Debug` via `as_str()`.
macro_rules! str_fmt {
    ($name:ident) => {
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(self.as_str(), f)
            }
        }
    };
}

/// Comparison and hashing via `as_str()`.
macro_rules! str_eq {
    ($name:ident) => {
        impl PartialEq for $name {
            fn eq(&self, other: &$name) -> bool {
                self.as_str() == other.as_str()
            }
        }

        impl Eq for $name {}

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.as_str() == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.as_str() == *other
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.as_str()
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.as_str()
            }
        }

        impl std::hash::Hash for $name {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.as_str().hash(state)
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &$name) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl Ord for $name {
            fn cmp(&self, other: &$name) -> std::cmp::Ordering {
                self.as_str().cmp(other.as_str())
            }
        }
    };
}

mod authority;
mod builder;
mod chars;
mod error;
mod host;
mod idna;
mod parse;
mod path;
mod query;
pub mod quoting;
mod scheme;
mod url;

pub use crate::authority::{Authority, Port, UserInfo};
pub use crate::builder::{Builder, Parts};
pub use crate::error::{ErrorKind, InvalidUrl, InvalidUrlParts};
pub use crate::host::Host;
pub use crate::path::{Path, PathAndQuery, Segments};
pub use crate::query::{Fragment, Query, QueryPairs};
pub use crate::scheme::Scheme;
pub use crate::url::Url;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
