//! A utl-8 encoded read-only string with Bytes as a storage.
use std::convert::TryFrom;
use std::{borrow, fmt, hash, ops, str};

use bytes::Bytes;

/// A utf-8 encoded string with [`Bytes`] as a storage.
///
/// [`Bytes`]: https://docs.rs/bytes/0.5.3/bytes/struct.Bytes.html
#[derive(Clone, Eq, Ord, PartialOrd, Default)]
pub struct ByteString(Bytes);

impl ByteString {
    /// Creates a new `ByteString`.
    pub fn new() -> Self {
        ByteString(Bytes::new())
    }

    /// Get a reference to the underlying bytes object.
    pub fn get_ref(&self) -> &Bytes {
        &self.0
    }

    /// Unwraps this `ByteString`, returning the underlying bytes object.
    pub fn into_inner(self) -> Bytes {
        self.0
    }

    /// Creates a new `ByteString` from a static str.
    pub const fn from_static(src: &'static str) -> ByteString {
        Self(Bytes::from_static(src.as_bytes()))
    }

    /// Creates a new `ByteString` from a Bytes.
    pub const unsafe fn from_bytes_unchecked(src: Bytes) -> ByteString {
        Self(src)
    }
}

impl PartialEq<str> for ByteString {
    fn eq(&self, other: &str) -> bool {
        &self[..] == other
    }
}

impl<T: AsRef<str>> PartialEq<T> for ByteString {
    fn eq(&self, other: &T) -> bool {
        &self[..] == other.as_ref()
    }
}

impl AsRef<[u8]> for ByteString {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }
}

impl AsRef<str> for ByteString {
    fn as_ref(&self) -> &str {
        &*self
    }
}

impl hash::Hash for ByteString {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl ops::Deref for ByteString {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        let b = self.0.as_ref();
        unsafe { str::from_utf8_unchecked(b) }
    }
}

impl borrow::Borrow<str> for ByteString {
    fn borrow(&self) -> &str {
        &*self
    }
}

impl From<String> for ByteString {
    fn from(value: String) -> Self {
        Self(Bytes::from(value))
    }
}

impl<'a> From<&'a str> for ByteString {
    fn from(value: &'a str) -> Self {
        Self(Bytes::copy_from_slice(value.as_ref()))
    }
}

impl<'a> TryFrom<&'a [u8]> for ByteString {
    type Error = str::Utf8Error;

    fn try_from(value: &'a [u8]) -> Result<Self, Self::Error> {
        let _ = str::from_utf8(value)?;
        Ok(ByteString(Bytes::copy_from_slice(value)))
    }
}

impl TryFrom<Vec<u8>> for ByteString {
    type Error = str::Utf8Error;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        let _ = str::from_utf8(value.as_ref())?;
        Ok(ByteString(Bytes::from(value)))
    }
}

impl TryFrom<Bytes> for ByteString {
    type Error = str::Utf8Error;

    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
        let _ = str::from_utf8(value.as_ref())?;
        Ok(ByteString(value))
    }
}

impl TryFrom<bytes::BytesMut> for ByteString {
    type Error = str::Utf8Error;

    fn try_from(value: bytes::BytesMut) -> Result<Self, Self::Error> {
        let _ = str::from_utf8(value.as_ref())?;
        Ok(ByteString(value.freeze()))
    }
}

macro_rules! array_impls {
    ($($len:expr)+) => {
        $(
            impl<'a> TryFrom<&'a [u8; $len]> for ByteString {
                type Error = str::Utf8Error;

                fn try_from(value: &'a [u8; $len]) -> Result<Self, Self::Error> {
                    ByteString::try_from(&value[..])
                }
            }
        )+
    }
}

array_impls!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16);

impl fmt::Debug for ByteString {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        (**self).fmt(fmt)
    }
}

impl fmt::Display for ByteString {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        (**self).fmt(fmt)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    #[test]
    fn test_partial_eq() {
        let s: ByteString = ByteString::from_static("test");
        assert_eq!(s, "test");
        assert_eq!(s, *"test");
        assert_eq!(s, "test".to_string());
    }

    #[test]
    fn test_new() {
        let _: ByteString = ByteString::new();
    }

    #[test]
    fn test_hash() {
        let mut hasher1 = DefaultHasher::default();
        "str".hash(&mut hasher1);

        let mut hasher2 = DefaultHasher::default();
        let s = ByteString::from_static("str");
        s.hash(&mut hasher2);
        assert_eq!(hasher1.finish(), hasher2.finish());
    }

    #[test]
    fn test_from_string() {
        let s: ByteString = "hello".to_string().into();
        assert_eq!(&s, "hello");
        let t: &str = s.as_ref();
        assert_eq!(t, "hello");
    }

    #[test]
    fn test_from_str() {
        let _: ByteString = "str".into();
    }

    #[test]
    fn test_from_static_str() {
        const _S: ByteString = ByteString::from_static("hello");
        let _ = ByteString::from_static("str");
    }

    #[test]
    fn test_try_from_rbytes() {
        let _ = ByteString::try_from(b"nice bytes").unwrap();
    }

    #[test]
    fn test_try_from_bytes() {
        let _ = ByteString::try_from(Bytes::from_static(b"nice bytes")).unwrap();
    }

    #[test]
    fn test_try_from_bytesmut() {
        let _ = ByteString::try_from(bytes::BytesMut::from(&b"nice bytes"[..])).unwrap();
    }
}
