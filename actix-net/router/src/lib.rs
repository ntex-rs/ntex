//! Resource path matching library.
mod de;
mod path;
mod resource;
mod router;

pub use self::de::PathDeserializer;
pub use self::path::Path;
pub use self::resource::ResourceDef;
pub use self::router::{ResourceInfo, Router, RouterBuilder};

pub trait Resource<T: ResourcePath> {
    fn resource_path(&mut self) -> &mut Path<T>;
}

pub trait ResourcePath {
    fn path(&self) -> &str;
}

impl ResourcePath for String {
    fn path(&self) -> &str {
        self.as_str()
    }
}

impl<'a> ResourcePath for &'a str {
    fn path(&self) -> &str {
        self
    }
}

impl ResourcePath for bytestring::ByteString {
    fn path(&self) -> &str {
        &*self
    }
}

/// Helper trait for type that could be converted to path pattern
pub trait IntoPattern {
    /// Signle patter
    fn is_single(&self) -> bool;

    fn patterns(&self) -> Vec<String>;
}

impl IntoPattern for String {
    fn is_single(&self) -> bool {
        true
    }

    fn patterns(&self) -> Vec<String> {
        vec![self.clone()]
    }
}

impl<'a> IntoPattern for &'a String {
    fn is_single(&self) -> bool {
        true
    }

    fn patterns(&self) -> Vec<String> {
        vec![self.as_str().to_string()]
    }
}

impl<'a> IntoPattern for &'a str {
    fn is_single(&self) -> bool {
        true
    }

    fn patterns(&self) -> Vec<String> {
        vec![self.to_string()]
    }
}

impl<T: AsRef<str>> IntoPattern for Vec<T> {
    fn is_single(&self) -> bool {
        self.len() == 1
    }

    fn patterns(&self) -> Vec<String> {
        self.into_iter().map(|v| v.as_ref().to_string()).collect()
    }
}

macro_rules! array_patterns (($tp:ty, $num:tt) => {
    impl IntoPattern for [$tp; $num] {
        fn is_single(&self) -> bool {
            $num == 1
        }

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

#[cfg(feature = "http")]
mod url;

#[cfg(feature = "http")]
pub use self::url::{Quoter, Url};

#[cfg(feature = "http")]
mod http_support {
    use super::ResourcePath;
    use http::Uri;

    impl ResourcePath for Uri {
        fn path(&self) -> &str {
            self.path()
        }
    }
}
