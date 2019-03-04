//! Resource path matching library.
mod de;
mod path;
mod resource;
mod router;

pub use self::de::PathDeserializer;
pub use self::path::Path;
pub use self::resource::ResourceDef;
pub use self::router::{ResourceInfo, Router, RouterBuilder};

pub trait Resource {
    fn path(&self) -> &str;
}

impl Resource for String {
    fn path(&self) -> &str {
        self.as_str()
    }
}

impl<'a> Resource for &'a str {
    fn path(&self) -> &str {
        self
    }
}

impl<T: AsRef<[u8]>> Resource for string::String<T> {
    fn path(&self) -> &str {
        &*self
    }
}

#[cfg(feature = "http")]
mod url;

#[cfg(feature = "http")]
pub use self::url::Url;

#[cfg(feature = "http")]
mod http_support {
    use super::Resource;
    use http::Uri;

    impl Resource for Uri {
        fn path(&self) -> &str {
            self.path()
        }
    }
}
