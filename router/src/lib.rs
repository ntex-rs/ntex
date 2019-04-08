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

impl<T: AsRef<[u8]>> ResourcePath for string::String<T> {
    fn path(&self) -> &str {
        &*self
    }
}

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
