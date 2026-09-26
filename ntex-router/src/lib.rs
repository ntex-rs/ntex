#![deny(clippy::pedantic)]
#![allow(
    clippy::must_use_candidate,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::iter_without_into_iter,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::too_many_lines
)]

//! Resource path matching library.
//!
//! A [`Router`] maps request paths to values. Resources are registered with
//! a [`RouterBuilder`] from path patterns, see [`ResourceDef`]. Matching a
//! [`Path`] returns the value of the first registered resource that matches,
//! and stores the values of the pattern's dynamic segments in the `Path`.
//!
//! # Pattern syntax
//!
//! Patterns are split into segments by `/`.
//!
//! * `/users/list` — static segments match literally. A trailing `/` is
//!   significant, `/users/` does not match `/users`.
//! * `/users/{id}` — a dynamic segment matches one or more characters up to
//!   the next `/` and stores them as `id`.
//! * `/files/{name}.{ext}` — a segment can combine static text and several
//!   dynamic parts.
//! * `/users/{id:[0-9]+}` — a dynamic segment with a custom regular
//!   expression.
//! * `/static/{tail}*` — a tail segment matches the rest of the path,
//!   including `/`. Custom regular expressions are not supported for tails.
//! * `/static/*` — a static tail matches the rest of the path without storing
//!   it.
//!
//! [`ResourceDef::prefix()`] creates a resource that matches paths starting
//! with the pattern, at a segment boundary. After a match, [`Path::path()`]
//! returns the rest of the path.
//!
//! # Example
//!
//! ```
//! use ntex_router::{Path, Router};
//!
//! let mut builder = Router::<&str>::builder();
//! builder.path("/users/{id}", "user");
//! builder.path("/files/{tail}*", "files");
//! let router = builder.build();
//!
//! let mut path = Path::new("/users/42");
//! let (value, _) = router.recognize(&mut path).unwrap();
//! assert_eq!(*value, "user");
//! assert_eq!(path.get("id"), Some("42"));
//!
//! let mut path = Path::new("/files/css/site.css");
//! let (value, _) = router.recognize(&mut path).unwrap();
//! assert_eq!(*value, "files");
//! assert_eq!(&path["tail"], "css/site.css");
//!
//! assert!(router.recognize(&mut Path::new("/unknown")).is_none());
//! ```
#![warn(missing_docs)]
mod de;
mod path;
mod resource;
mod router;
mod tree;

pub use self::de::PathDeserializer;
pub use self::path::{Path, PathIter};
pub use self::resource::ResourceDef;
pub use self::router::{ResourceId, Router, RouterBuilder};

#[doc(hidden)]
#[derive(Debug)]
pub struct ResourceInfo;

/// A value that can be matched by a [`Router`].
///
/// [`Path`] implements it, a request type can implement it to be matched
/// directly.
pub trait Resource<T: ResourcePath> {
    /// Path to match.
    fn path(&self) -> &str;

    /// Path state that stores match results.
    fn resource_path(&mut self) -> &mut Path<T>;
}

/// A path source, e.g. a string or an `http::Uri`.
pub trait ResourcePath {
    /// Full path.
    fn path(&self) -> &str;

    /// Decodes a path segment before it is matched.
    ///
    /// The path is split into segments on `/` before decoding, so a decoded
    /// segment may contain `/`. The default implementation returns the
    /// segment unchanged.
    fn unquote(s: &str) -> std::borrow::Cow<'_, str> {
        s.into()
    }
}

impl ResourcePath for String {
    fn path(&self) -> &str {
        self.as_str()
    }
}

impl ResourcePath for &str {
    fn path(&self) -> &str {
        self
    }
}

impl ResourcePath for ntex_bytes::ByteString {
    fn path(&self) -> &str {
        self
    }
}

impl<T: ResourcePath> ResourcePath for &T {
    fn path(&self) -> &str {
        (*self).path()
    }
}

/// Helper trait for type that could be converted to path patterns.
///
/// Implemented for strings, and for vectors and arrays of strings for
/// resources with several patterns.
pub trait IntoPattern {
    /// Path patterns.
    fn patterns(&self) -> Vec<String>;
}

impl IntoPattern for String {
    fn patterns(&self) -> Vec<String> {
        vec![self.clone()]
    }
}

impl IntoPattern for &String {
    fn patterns(&self) -> Vec<String> {
        vec![self.as_str().to_string()]
    }
}

impl IntoPattern for &str {
    fn patterns(&self) -> Vec<String> {
        vec![(*self).to_string()]
    }
}

impl<T: AsRef<str>> IntoPattern for Vec<T> {
    fn patterns(&self) -> Vec<String> {
        self.iter().map(|v| v.as_ref().to_string()).collect()
    }
}

macro_rules! array_patterns (($tp:ty, $num:tt) => {
    impl IntoPattern for [$tp; $num] {
        fn patterns(&self) -> Vec<String> {
            self.iter().map(|v| v.to_string()).collect()
        }
    }
});

array_patterns!(&str, 1);
array_patterns!(&str, 2);
array_patterns!(&str, 3);
array_patterns!(&str, 4);
array_patterns!(&str, 5);
array_patterns!(&str, 6);
array_patterns!(&str, 7);
array_patterns!(&str, 8);
array_patterns!(&str, 9);
array_patterns!(&str, 10);
array_patterns!(&str, 11);
array_patterns!(&str, 12);
array_patterns!(&str, 13);
array_patterns!(&str, 14);
array_patterns!(&str, 15);
array_patterns!(&str, 16);

array_patterns!(String, 1);
array_patterns!(String, 2);
array_patterns!(String, 3);
array_patterns!(String, 4);
array_patterns!(String, 5);
array_patterns!(String, 6);
array_patterns!(String, 7);
array_patterns!(String, 8);
array_patterns!(String, 9);
array_patterns!(String, 10);
array_patterns!(String, 11);
array_patterns!(String, 12);
array_patterns!(String, 13);
array_patterns!(String, 14);
array_patterns!(String, 15);
array_patterns!(String, 16);

mod quoter;

#[cfg(feature = "http")]
mod http_support {
    use super::ResourcePath;
    use http::Uri;

    /// Path segments are percent-decoded, segments that would not decode to
    /// valid utf-8 are kept percent-encoded.
    ///
    /// Segments are decoded after the path is split on `/`, so parameter
    /// values can contain any character, including `/` and `%`. For example
    /// `/files/..%2F..%2Fetc` matches `/files/{name}` with `name` set to
    /// `../../etc`. Such values must be validated before they are used, e.g.
    /// as a file system path.
    impl ResourcePath for Uri {
        fn path(&self) -> &str {
            self.path()
        }

        fn unquote(s: &str) -> std::borrow::Cow<'_, str> {
            if let Some(q) = super::quoter::requote(s.as_bytes()) {
                std::borrow::Cow::Owned(q)
            } else {
                std::borrow::Cow::Borrowed(s)
            }
        }
    }
}
