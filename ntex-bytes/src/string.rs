//! A UTF-8 encoded read-only string using Bytes as storage.
use std::{borrow, convert::TryFrom, fmt, hash, ops, str};

use crate::Bytes;

/// An immutable UTF-8 encoded string with [`Bytes`] as a storage.
#[derive(Clone, Default, Eq, PartialOrd, Ord)]
pub struct ByteString(Bytes);

impl ByteString {
    /// Creates a new empty `ByteString`.
    #[inline]
    pub const fn new() -> Self {
        ByteString(Bytes::new())
    }

    /// Get a reference to the underlying `Bytes` object.
    #[inline]
    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    /// Unwraps this `ByteString` into the underlying `Bytes` object.
    #[inline]
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    /// Creates a new `ByteString` from a `&'static str`.
    #[inline]
    pub const fn from_static(src: &'static str) -> ByteString {
        Self(Bytes::from_static(src.as_bytes()))
    }

    /// Shortens the buffer to `len` bytes and dropping the rest.
    #[inline]
    pub fn trimdown(&mut self) {
        self.0.trimdown()
    }

    /// Creates a new `ByteString` from a Bytes.
    ///
    /// # Safety
    /// This function is unsafe because it does not check the bytes passed to it are valid UTF-8.
    /// If this constraint is violated, it may cause memory unsafety issues with future users of
    /// the `ByteString`, as we assume that `ByteString`s are valid UTF-8. However, the most likely
    /// issue is that the data gets corrupted.
    #[inline]
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

impl AsRef<str> for ByteString {
    #[inline]
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
        let bytes = self.0.as_ref();
        // SAFETY:
        // UTF-8 validity is guaranteed at during construction.
        unsafe { str::from_utf8_unchecked(bytes) }
    }
}

impl borrow::Borrow<str> for ByteString {
    #[inline]
    fn borrow(&self) -> &str {
        &*self
    }
}

impl From<String> for ByteString {
    #[inline]
    fn from(value: String) -> Self {
        Self(Bytes::from(value))
    }
}

impl From<&str> for ByteString {
    #[inline]
    fn from(value: &str) -> Self {
        Self(Bytes::copy_from_slice(value.as_ref()))
    }
}

impl<'a> From<borrow::Cow<'a, str>> for ByteString {
    #[inline]
    fn from(value: borrow::Cow<'a, str>) -> Self {
        Self::from(value.to_owned())
    }
}

impl TryFrom<&[u8]> for ByteString {
    type Error = str::Utf8Error;

    #[inline]
    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        let _ = str::from_utf8(value)?;
        Ok(ByteString(Bytes::copy_from_slice(value)))
    }
}

impl TryFrom<Vec<u8>> for ByteString {
    type Error = str::Utf8Error;

    #[inline]
    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        let buf = String::from_utf8(value).map_err(|err| err.utf8_error())?;
        Ok(ByteString(Bytes::from(buf)))
    }
}

impl TryFrom<Bytes> for ByteString {
    type Error = str::Utf8Error;

    #[inline]
    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
        let _ = str::from_utf8(value.as_ref())?;
        Ok(ByteString(value))
    }
}

impl TryFrom<crate::BytesMut> for ByteString {
    type Error = str::Utf8Error;

    #[inline]
    fn try_from(value: crate::BytesMut) -> Result<Self, Self::Error> {
        let _ = str::from_utf8(&value)?;
        Ok(ByteString(value.freeze()))
    }
}

macro_rules! array_impls {
    ($($len:expr)+) => {
        $(
            impl TryFrom<[u8; $len]> for ByteString {
                type Error = str::Utf8Error;

                #[inline]
                fn try_from(value: [u8; $len]) -> Result<Self, Self::Error> {
                    ByteString::try_from(&value[..])
                }
            }

            impl TryFrom<&[u8; $len]> for ByteString {
                type Error = str::Utf8Error;

                #[inline]
                fn try_from(value: &[u8; $len]) -> Result<Self, Self::Error> {
                    ByteString::try_from(&value[..])
                }
            }
        )+
    }
}

array_impls!(0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);

impl fmt::Debug for ByteString {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(fmt)
    }
}

impl fmt::Display for ByteString {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(fmt)
    }
}

#[cfg(feature = "serde")]
mod serde {
    use alloc::string::String;

    use serde::de::{Deserialize, Deserializer};
    use serde::ser::{Serialize, Serializer};

    use super::ByteString;

    impl Serialize for ByteString {
        #[inline]
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_str(self.as_ref())
        }
    }

    impl<'de> Deserialize<'de> for ByteString {
        #[inline]
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            String::deserialize(deserializer).map(ByteString::from)
        }
    }
}

#[cfg(test)]
mod test {
    use alloc::borrow::ToOwned;
    use core::hash::{Hash, Hasher};

    use ahash::AHasher;

    use super::*;

    #[test]
    fn test_partial_eq() {
        let s: ByteString = ByteString::from_static("test");
        assert_eq!(s, "test");
        assert_eq!(s, *"test");
        assert_eq!(s, "test".to_owned());
    }

    #[test]
    fn test_new() {
        let _: ByteString = ByteString::new();
    }

    #[test]
    fn test_hash() {
        let mut hasher1 = AHasher::default();
        "str".hash(&mut hasher1);

        let mut hasher2 = AHasher::default();
        let s = ByteString::from_static("str");
        s.hash(&mut hasher2);
        assert_eq!(hasher1.finish(), hasher2.finish());
    }

    #[test]
    fn test_from_string() {
        let s: ByteString = "hello".to_owned().into();
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
        static _S: ByteString = ByteString::from_static("hello");
        let _ = ByteString::from_static("str");
    }

    #[test]
    fn test_try_from_slice() {
        let _ = ByteString::try_from(b"nice bytes").unwrap();
    }

    #[test]
    fn test_try_from_array() {
        assert_eq!(
            ByteString::try_from([b'h', b'i']).unwrap(),
            ByteString::from_static("hi")
        );
    }

    #[test]
    fn test_try_from_bytes() {
        let _ = ByteString::try_from(Bytes::from_static(b"nice bytes")).unwrap();
    }

    #[test]
    fn test_try_from_bytes_mut() {
        let _ = ByteString::try_from(bytes::BytesMut::from(&b"nice bytes"[..])).unwrap();
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_serialize() {
        let s: ByteString = serde_json::from_str(r#""nice bytes""#).unwrap();
        assert_eq!(s, "nice bytes");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_deserialize() {
        let s = serde_json::to_string(&ByteString::from_static("nice bytes")).unwrap();
        assert_eq!(s, r#""nice bytes""#);
    }
}
