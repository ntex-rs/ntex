//! A UTF-8 encoded read-only string using Bytes as storage.
use std::{borrow, fmt, hash, ops, slice, str};

use crate::{Bytes, BytesMut, BytesVec};

/// An immutable UTF-8 encoded string with [`Bytes`] as a storage.
#[derive(Clone, Default, Eq, PartialOrd, Ord)]
pub struct ByteString(Bytes);

impl ByteString {
    /// Creates a new empty `ByteString`.
    #[inline]
    pub const fn new() -> Self {
        ByteString(Bytes::new())
    }

    /// Get a str slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        self
    }

    /// Get a reference to the underlying bytes.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        self.0.as_ref()
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

    /// Returns a slice of self for the provided range.
    ///
    /// This will increment the reference count for the underlying memory and
    /// return a new `ByteString` handle set to the slice.
    ///
    /// This operation is `O(1)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::ByteString;
    ///
    /// let a = ByteString::from("hello world");
    /// let b = a.slice(2..5);
    ///
    /// assert_eq!(b, "llo");
    /// ```
    ///
    /// # Panics
    ///
    /// Requires that `begin <= end` and `end <= self.len()`, otherwise slicing
    /// will panic.
    pub fn slice(
        &self,
        range: impl ops::RangeBounds<usize> + slice::SliceIndex<str> + Clone,
    ) -> ByteString {
        ops::Index::index(self.as_ref(), range.clone());
        ByteString(self.0.slice(range))
    }

    /// Splits the bytestring into two at the given index.
    ///
    /// Afterwards `self` contains elements `[0, at)`, and the returned `ByteString`
    /// contains elements `[at, len)`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::ByteString;
    ///
    /// let mut a = ByteString::from("hello world");
    /// let b = a.split_off(5);
    ///
    /// assert_eq!(a, "hello");
    /// assert_eq!(b, " world");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `at > len`.
    pub fn split_off(&mut self, at: usize) -> ByteString {
        // check str
        let _ = self.split_at(at);

        ByteString(self.0.split_off(at))
    }

    /// Splits the bytestring into two at the given index.
    ///
    /// Afterwards `self` contains elements `[at, len)`, and the returned
    /// `Bytes` contains elements `[0, at)`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::ByteString;
    ///
    /// let mut a = ByteString::from("hello world");
    /// let b = a.split_to(5);
    ///
    /// assert_eq!(a, " world");
    /// assert_eq!(b, "hello");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `at > len`.
    pub fn split_to(&mut self, at: usize) -> ByteString {
        // check str
        let _ = self.split_at(at);

        ByteString(self.0.split_to(at))
    }

    /// Shortens the buffer to `len` bytes and dropping the rest.
    #[inline]
    pub fn trimdown(&mut self) {
        self.0.trimdown()
    }

    /// Clears the buffer, removing all data.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::ByteString;
    ///
    /// let mut a = ByteString::from("hello world");
    /// a.clear();
    ///
    /// assert!(a.is_empty());
    /// ```
    #[inline]
    pub fn clear(&mut self) {
        self.0.clear()
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
        self
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
        // UTF-8 validity is guaranteed during construction.
        unsafe { str::from_utf8_unchecked(bytes) }
    }
}

impl borrow::Borrow<str> for ByteString {
    #[inline]
    fn borrow(&self) -> &str {
        self
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

impl From<&ByteString> for ByteString {
    #[inline]
    fn from(value: &ByteString) -> Self {
        value.clone()
    }
}

impl<'a> From<borrow::Cow<'a, str>> for ByteString {
    #[inline]
    fn from(value: borrow::Cow<'a, str>) -> Self {
        match value {
            borrow::Cow::Owned(s) => Self::from(s),
            borrow::Cow::Borrowed(s) => Self::from(s),
        }
    }
}

impl TryFrom<&[u8]> for ByteString {
    type Error = ();

    #[inline]
    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if utf8::is_valid(value) {
            Ok(ByteString(Bytes::copy_from_slice(value)))
        } else {
            Err(())
        }
    }
}

impl TryFrom<Vec<u8>> for ByteString {
    type Error = ();

    #[inline]
    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        if utf8::is_valid(&value) {
            Ok(ByteString(Bytes::from(value)))
        } else {
            Err(())
        }
    }
}

impl TryFrom<Bytes> for ByteString {
    type Error = ();

    #[inline]
    fn try_from(value: Bytes) -> Result<Self, Self::Error> {
        if utf8::is_valid(&value) {
            Ok(ByteString(value))
        } else {
            Err(())
        }
    }
}

impl TryFrom<&Bytes> for ByteString {
    type Error = ();

    #[inline]
    fn try_from(value: &Bytes) -> Result<Self, Self::Error> {
        if utf8::is_valid(value) {
            Ok(ByteString(value.clone()))
        } else {
            Err(())
        }
    }
}

impl TryFrom<BytesMut> for ByteString {
    type Error = ();

    #[inline]
    fn try_from(value: BytesMut) -> Result<Self, Self::Error> {
        if utf8::is_valid(&value) {
            Ok(ByteString(value.freeze()))
        } else {
            Err(())
        }
    }
}

impl TryFrom<BytesVec> for ByteString {
    type Error = ();

    #[inline]
    fn try_from(value: BytesVec) -> Result<Self, Self::Error> {
        if utf8::is_valid(&value) {
            Ok(ByteString(value.freeze()))
        } else {
            Err(())
        }
    }
}

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

mod serde {
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

#[cfg(feature = "simd")]
mod utf8 {
    pub(super) fn is_valid(input: &[u8]) -> bool {
        simdutf8::basic::from_utf8(input).is_ok()
    }
}

#[cfg(not(feature = "simd"))]
mod utf8 {
    pub(super) fn is_valid(input: &[u8]) -> bool {
        std::str::from_utf8(input).is_ok()
    }
}

#[cfg(test)]
mod test {
    use std::borrow::{Borrow, Cow};
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    use super::*;

    #[test]
    fn test_basics() {
        let mut s = ByteString::from_static("test");
        s.trimdown();
        assert_eq!(s, "test");
        assert_eq!(s, *"test");
        assert_eq!(s, "test".to_owned());
        assert_eq!(s.as_str(), "test");
        assert_eq!(s.as_slice(), b"test");
        assert_eq!(s.as_bytes(), &Bytes::copy_from_slice(b"test"));
        assert_eq!(Borrow::<str>::borrow(&s), "test");

        assert_eq!(format!("{}", s), "test");
        assert_eq!(format!("{:?}", s), "\"test\"");

        let b = s.into_bytes();
        assert_eq!(b, Bytes::copy_from_slice(b"test"));

        let s = unsafe { ByteString::from_bytes_unchecked(b) };
        assert_eq!(s, "test");
        assert_eq!(s.slice(0..2), "te");

        let s = ByteString::from(Cow::Borrowed("test"));
        assert_eq!(s, "test");
        let mut s = ByteString::from(Cow::Owned("test".to_string()));
        assert_eq!(s, "test");

        s.clear();
        assert_eq!(s, "");
    }

    #[test]
    fn test_split() {
        let mut s = ByteString::from_static("helloworld");
        let s1 = s.split_off(5);
        assert_eq!(s, "hello");
        assert_eq!(s1, "world");

        let mut s = ByteString::from_static("helloworld");
        let s1 = s.split_to(5);
        assert_eq!(s, "world");
        assert_eq!(s1, "hello");
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
    fn test_from() {
        // String
        let s: ByteString = "hello".to_owned().into();
        assert_eq!(&s, "hello");
        let t: &str = s.as_ref();
        assert_eq!(t, "hello");

        // str
        let _: ByteString = "str".into();

        // static str
        static _S: ByteString = ByteString::from_static("hello");
        let _ = ByteString::from_static("str");

        let s = ByteString::from_static("hello");
        let s1 = ByteString::from(&s);
        assert_eq!(s1, "hello");
    }

    #[test]
    fn test_try_from() {
        let _ = ByteString::try_from(&b"nice bytes"[..]).unwrap();
        assert!(ByteString::try_from(b"\xc3\x28".as_ref()).is_err());

        let _ = ByteString::try_from(b"nice bytes".to_vec()).unwrap();
        assert!(ByteString::try_from(vec![b'\xc3']).is_err());

        let _ = ByteString::try_from(Bytes::from_static(b"nice bytes")).unwrap();
        assert!(ByteString::try_from(Bytes::from_static(b"\xc3\x28")).is_err());

        let _ = ByteString::try_from(&Bytes::from_static(b"nice bytes")).unwrap();
        assert!(ByteString::try_from(&Bytes::from_static(b"\xc3\x28")).is_err());

        let _ = ByteString::try_from(BytesMut::from(&b"nice bytes"[..])).unwrap();
        assert!(ByteString::try_from(BytesMut::copy_from_slice(b"\xc3\x28")).is_err());

        let _ =
            ByteString::try_from(BytesVec::copy_from_slice(&b"nice bytes"[..])).unwrap();
        assert!(ByteString::try_from(BytesVec::copy_from_slice(b"\xc3\x28")).is_err());
    }

    #[test]
    fn test_serialize() {
        let s: ByteString = serde_json::from_str(r#""nice bytes""#).unwrap();
        assert_eq!(s, "nice bytes");
    }

    #[test]
    fn test_deserialize() {
        let s = serde_json::to_string(&ByteString::from_static("nice bytes")).unwrap();
        assert_eq!(s, r#""nice bytes""#);
    }
}
