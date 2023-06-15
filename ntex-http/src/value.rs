#![allow(
    clippy::should_implement_trait,
    clippy::no_effect,
    clippy::missing_safety_doc
)]
use std::{cmp, error::Error, fmt, str, str::FromStr};

use ntex_bytes::{ByteString, Bytes};

#[allow(clippy::derived_hash_with_manual_eq)]
/// Represents an HTTP header field value.
///
/// In practice, HTTP header field values are usually valid ASCII. However, the
/// HTTP spec allows for a header value to contain opaque bytes as well. In this
/// case, the header field value is not able to be represented as a string.
///
/// To handle this, the `HeaderValue` is useable as a type and can be compared
/// with strings and implements `Debug`. A `to_str` fn is provided that returns
/// an `Err` if the header value contains non visible ascii characters.
#[derive(Clone, Hash, Eq)]
pub struct HeaderValue {
    inner: Bytes,
    is_sensitive: bool,
}

/// A possible error when converting a `HeaderValue` from a string or byte
/// slice.
pub struct InvalidHeaderValue {
    _priv: (),
}

/// A possible error when converting a `HeaderValue` to a string representation.
///
/// Header field values may contain opaque bytes, in which case it is not
/// possible to represent the value as a string.
#[derive(Debug)]
pub struct ToStrError {
    _priv: (),
}

impl HeaderValue {
    /// Convert a static string to a `HeaderValue`.
    ///
    /// This function will not perform any copying, however the string is
    /// checked to ensure that no invalid characters are present. Only visible
    /// ASCII characters (32-127) are permitted.
    ///
    /// # Panics
    ///
    /// This function panics if the argument contains invalid header value
    /// characters.
    ///
    /// Until [Allow panicking in constants](https://github.com/rust-lang/rfcs/pull/2345)
    /// makes its way into stable, the panic message at compile-time is
    /// going to look cryptic, but should at least point at your header value:
    ///
    /// ```text
    /// error: any use of this value will cause an error
    ///   --> http/src/header/value.rs:67:17
    ///    |
    /// 67 |                 ([] as [u8; 0])[0]; // Invalid header value
    ///    |                 ^^^^^^^^^^^^^^^^^^
    ///    |                 |
    ///    |                 index out of bounds: the length is 0 but the index is 0
    ///    |                 inside `HeaderValue::from_static` at http/src/header/value.rs:67:17
    ///    |                 inside `INVALID_HEADER` at src/main.rs:73:33
    ///    |
    ///   ::: src/main.rs:73:1
    ///    |
    /// 73 | const INVALID_HEADER: HeaderValue = HeaderValue::from_static("жsome value");
    ///    | ----------------------------------------------------------------------------
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_static("hello");
    /// assert_eq!(val, "hello");
    /// ```
    #[inline]
    #[allow(unconditional_panic)] // required for the panic circumvention
    pub const fn from_static(src: &'static str) -> HeaderValue {
        let bytes = src.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if !is_visible_ascii(bytes[i]) {
                ([] as [u8; 0])[0]; // Invalid header value
            }
            i += 1;
        }

        HeaderValue {
            inner: Bytes::from_static(bytes),
            is_sensitive: false,
        }
    }

    /// Attempt to convert a string to a `HeaderValue`.
    ///
    /// If the argument contains invalid header value characters, an error is
    /// returned. Only visible ASCII characters (32-127) are permitted. Use
    /// `from_bytes` to create a `HeaderValue` that includes opaque octets
    /// (128-255).
    ///
    /// This function is intended to be replaced in the future by a `TryFrom`
    /// implementation once the trait is stabilized in std.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_str("hello").unwrap();
    /// assert_eq!(val, "hello");
    /// ```
    ///
    /// An invalid value
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_str("\n");
    /// assert!(val.is_err());
    /// ```
    #[inline]
    pub fn from_str(src: &str) -> Result<HeaderValue, InvalidHeaderValue> {
        HeaderValue::try_from_generic(src, |s| Bytes::copy_from_slice(s.as_bytes()))
    }

    /// Attempt to convert a byte slice to a `HeaderValue`.
    ///
    /// If the argument contains invalid header value bytes, an error is
    /// returned. Only byte values between 32 and 255 (inclusive) are permitted,
    /// excluding byte 127 (DEL).
    ///
    /// This function is intended to be replaced in the future by a `TryFrom`
    /// implementation once the trait is stabilized in std.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_bytes(b"hello\xfa").unwrap();
    /// assert_eq!(val, &b"hello\xfa"[..]);
    /// ```
    ///
    /// An invalid value
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_bytes(b"\n");
    /// assert!(val.is_err());
    /// ```
    #[inline]
    pub fn from_bytes(src: &[u8]) -> Result<HeaderValue, InvalidHeaderValue> {
        HeaderValue::try_from_generic(src, Bytes::copy_from_slice)
    }

    /// Attempt to convert a `Bytes` buffer to a `HeaderValue`.
    ///
    /// This will try to prevent a copy if the type passed is the type used
    /// internally, and will copy the data if it is not.
    pub fn from_shared<T>(src: T) -> Result<HeaderValue, InvalidHeaderValue>
    where
        Bytes: From<T>,
    {
        let inner = Bytes::from(src);
        for &b in inner.as_ref() {
            if !is_valid(b) {
                return Err(InvalidHeaderValue { _priv: () });
            }
        }

        Ok(HeaderValue {
            inner,
            is_sensitive: false,
        })
    }

    /// Attempt to convert a `Bytes` buffer to a `HeaderValue`.
    ///
    /// This will try to prevent a copy if the type passed is the type used
    /// internally, and will copy the data if it is not.
    pub unsafe fn from_shared_unchecked(src: Bytes) -> HeaderValue {
        HeaderValue {
            inner: src,
            is_sensitive: false,
        }
    }

    #[deprecated]
    #[doc(hidden)]
    pub fn from_maybe_shared<T>(src: T) -> Result<HeaderValue, InvalidHeaderValue>
    where
        Bytes: From<T>,
    {
        HeaderValue::from_shared(src)
    }

    fn try_from_generic<T: AsRef<[u8]>, F: FnOnce(T) -> Bytes>(
        src: T,
        into: F,
    ) -> Result<HeaderValue, InvalidHeaderValue> {
        for &b in src.as_ref() {
            if !is_valid(b) {
                return Err(InvalidHeaderValue { _priv: () });
            }
        }
        Ok(HeaderValue {
            inner: into(src),
            is_sensitive: false,
        })
    }

    /// Yields a `&str` slice if the `HeaderValue` only contains visible ASCII
    /// chars.
    ///
    /// This function will perform a scan of the header value, checking all the
    /// characters.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_static("hello");
    /// assert_eq!(val.to_str().unwrap(), "hello");
    /// ```
    pub fn to_str(&self) -> Result<&str, ToStrError> {
        let bytes = self.as_ref();

        for &b in bytes {
            if !is_visible_ascii(b) {
                return Err(ToStrError { _priv: () });
            }
        }

        unsafe { Ok(str::from_utf8_unchecked(bytes)) }
    }

    /// Returns the length of `self`.
    ///
    /// This length is in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_static("hello");
    /// assert_eq!(val.len(), 5);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    /// Returns true if the `HeaderValue` has a length of zero bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_static("");
    /// assert!(val.is_empty());
    ///
    /// let val = HeaderValue::from_static("hello");
    /// assert!(!val.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Converts a `HeaderValue` to a byte slice.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let val = HeaderValue::from_static("hello");
    /// assert_eq!(val.as_bytes(), b"hello");
    /// ```
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.as_ref()
    }

    /// Converts a `HeaderValue` to a bytes object.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// # use ntex_bytes::Bytes;
    /// let val = HeaderValue::from_static("hello");
    /// assert_eq!(val.as_shared(), &Bytes::from_static(b"hello"));
    /// ```
    #[inline]
    pub fn as_shared(&self) -> &Bytes {
        &self.inner
    }

    /// Mark that the header value represents sensitive information.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let mut val = HeaderValue::from_static("my secret");
    ///
    /// val.set_sensitive(true);
    /// assert!(val.is_sensitive());
    ///
    /// val.set_sensitive(false);
    /// assert!(!val.is_sensitive());
    /// ```
    #[inline]
    pub fn set_sensitive(&mut self, val: bool) {
        self.is_sensitive = val;
    }

    /// Returns `true` if the value represents sensitive data.
    ///
    /// Sensitive data could represent passwords or other data that should not
    /// be stored on disk or in memory. By marking header values as sensitive,
    /// components using this crate can be instructed to treat them with special
    /// care for security reasons. For example, caches can avoid storing
    /// sensitive values, and HPACK encoders used by HTTP/2.0 implementations
    /// can choose not to compress them.
    ///
    /// Additionally, sensitive values will be masked by the `Debug`
    /// implementation of `HeaderValue`.
    ///
    /// Note that sensitivity is not factored into equality or ordering.
    ///
    /// # Examples
    ///
    /// ```
    /// # use ntex_http::header::HeaderValue;
    /// let mut val = HeaderValue::from_static("my secret");
    ///
    /// val.set_sensitive(true);
    /// assert!(val.is_sensitive());
    ///
    /// val.set_sensitive(false);
    /// assert!(!val.is_sensitive());
    /// ```
    #[inline]
    pub fn is_sensitive(&self) -> bool {
        self.is_sensitive
    }
}

impl AsRef<[u8]> for HeaderValue {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl fmt::Debug for HeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_sensitive {
            f.write_str("Sensitive")
        } else {
            f.write_str("\"")?;
            let mut from = 0;
            let bytes = self.as_bytes();
            for (i, &b) in bytes.iter().enumerate() {
                if !is_visible_ascii(b) || b == b'"' {
                    if from != i {
                        f.write_str(unsafe { str::from_utf8_unchecked(&bytes[from..i]) })?;
                    }
                    if b == b'"' {
                        f.write_str("\\\"")?;
                    } else {
                        write!(f, "\\x{:x}", b)?;
                    }
                    from = i + 1;
                }
            }

            f.write_str(unsafe { str::from_utf8_unchecked(&bytes[from..]) })?;
            f.write_str("\"")
        }
    }
}

impl FromStr for HeaderValue {
    type Err = InvalidHeaderValue;

    #[inline]
    fn from_str(s: &str) -> Result<HeaderValue, Self::Err> {
        HeaderValue::from_str(s)
    }
}

impl<'a> From<&'a HeaderValue> for HeaderValue {
    #[inline]
    fn from(t: &'a HeaderValue) -> Self {
        t.clone()
    }
}

impl From<http::header::HeaderValue> for HeaderValue {
    #[inline]
    fn from(t: http::header::HeaderValue) -> Self {
        let inner = Bytes::copy_from_slice(t.as_ref());
        HeaderValue {
            inner,
            is_sensitive: t.is_sensitive(),
        }
    }
}

impl<'a> From<&'a http::header::HeaderValue> for HeaderValue {
    #[inline]
    fn from(t: &'a http::header::HeaderValue) -> Self {
        let inner = Bytes::copy_from_slice(t.as_ref());
        HeaderValue {
            inner,
            is_sensitive: t.is_sensitive(),
        }
    }
}

impl<'a> TryFrom<&'a str> for HeaderValue {
    type Error = InvalidHeaderValue;

    #[inline]
    fn try_from(t: &'a str) -> Result<Self, Self::Error> {
        t.parse()
    }
}

impl<'a> TryFrom<&'a String> for HeaderValue {
    type Error = InvalidHeaderValue;
    #[inline]
    fn try_from(s: &'a String) -> Result<Self, Self::Error> {
        Self::from_bytes(s.as_bytes())
    }
}

impl<'a> TryFrom<&'a ByteString> for HeaderValue {
    type Error = InvalidHeaderValue;
    #[inline]
    fn try_from(s: &'a ByteString) -> Result<Self, Self::Error> {
        Self::from_shared(s.as_bytes().clone())
    }
}

impl<'a> TryFrom<&'a [u8]> for HeaderValue {
    type Error = InvalidHeaderValue;

    #[inline]
    fn try_from(t: &'a [u8]) -> Result<Self, Self::Error> {
        HeaderValue::from_bytes(t)
    }
}

impl TryFrom<String> for HeaderValue {
    type Error = InvalidHeaderValue;

    #[inline]
    fn try_from(s: String) -> Result<Self, Self::Error> {
        HeaderValue::from_shared(s)
    }
}

impl TryFrom<ByteString> for HeaderValue {
    type Error = InvalidHeaderValue;
    #[inline]
    fn try_from(s: ByteString) -> Result<Self, Self::Error> {
        Self::from_shared(s.into_bytes())
    }
}

impl TryFrom<Vec<u8>> for HeaderValue {
    type Error = InvalidHeaderValue;

    #[inline]
    fn try_from(vec: Vec<u8>) -> Result<Self, Self::Error> {
        HeaderValue::from_shared(vec)
    }
}

impl From<HeaderValue> for http::header::HeaderValue {
    #[inline]
    fn from(t: HeaderValue) -> Self {
        let mut hdr = Self::from_bytes(t.as_bytes()).unwrap();
        hdr.set_sensitive(t.is_sensitive());
        hdr
    }
}

impl<'a> From<&'a HeaderValue> for http::header::HeaderValue {
    #[inline]
    fn from(t: &'a HeaderValue) -> Self {
        let mut hdr = Self::from_bytes(t.as_bytes()).unwrap();
        hdr.set_sensitive(t.is_sensitive());
        hdr
    }
}

const fn is_visible_ascii(b: u8) -> bool {
    b >= 32 && b < 127 || b == b'\t'
}

#[inline]
fn is_valid(b: u8) -> bool {
    b >= 32 && b != 127 || b == b'\t'
}

impl Error for InvalidHeaderValue {}

impl fmt::Debug for InvalidHeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("InvalidHeaderValue")
            // skip _priv noise
            .finish()
    }
}

impl fmt::Display for InvalidHeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("failed to parse header value")
    }
}

impl Error for ToStrError {}

impl fmt::Display for ToStrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("failed to convert header to a str")
    }
}

// ===== PartialEq / PartialOrd =====

impl PartialEq for HeaderValue {
    #[inline]
    fn eq(&self, other: &HeaderValue) -> bool {
        self.inner == other.inner
    }
}

impl PartialOrd for HeaderValue {
    #[inline]
    fn partial_cmp(&self, other: &HeaderValue) -> Option<cmp::Ordering> {
        self.inner.partial_cmp(&other.inner)
    }
}

impl Ord for HeaderValue {
    #[inline]
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.inner.cmp(&other.inner)
    }
}

impl PartialEq<str> for HeaderValue {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.inner == other.as_bytes()
    }
}

impl PartialEq<[u8]> for HeaderValue {
    #[inline]
    fn eq(&self, other: &[u8]) -> bool {
        self.inner == other
    }
}

impl PartialOrd<str> for HeaderValue {
    #[inline]
    fn partial_cmp(&self, other: &str) -> Option<cmp::Ordering> {
        (*self.inner).partial_cmp(other.as_bytes())
    }
}

impl PartialOrd<[u8]> for HeaderValue {
    #[inline]
    fn partial_cmp(&self, other: &[u8]) -> Option<cmp::Ordering> {
        (*self.inner).partial_cmp(other)
    }
}

impl PartialEq<HeaderValue> for str {
    #[inline]
    fn eq(&self, other: &HeaderValue) -> bool {
        *other == *self
    }
}

impl PartialEq<HeaderValue> for [u8] {
    #[inline]
    fn eq(&self, other: &HeaderValue) -> bool {
        *other == *self
    }
}

impl PartialOrd<HeaderValue> for str {
    #[inline]
    fn partial_cmp(&self, other: &HeaderValue) -> Option<cmp::Ordering> {
        self.as_bytes().partial_cmp(other.as_bytes())
    }
}

impl PartialOrd<HeaderValue> for [u8] {
    #[inline]
    fn partial_cmp(&self, other: &HeaderValue) -> Option<cmp::Ordering> {
        self.partial_cmp(other.as_bytes())
    }
}

impl PartialEq<String> for HeaderValue {
    #[inline]
    fn eq(&self, other: &String) -> bool {
        *self == other[..]
    }
}

impl PartialOrd<String> for HeaderValue {
    #[inline]
    fn partial_cmp(&self, other: &String) -> Option<cmp::Ordering> {
        self.inner.partial_cmp(other.as_bytes())
    }
}

impl PartialEq<HeaderValue> for String {
    #[inline]
    fn eq(&self, other: &HeaderValue) -> bool {
        *other == *self
    }
}

impl PartialOrd<HeaderValue> for String {
    #[inline]
    fn partial_cmp(&self, other: &HeaderValue) -> Option<cmp::Ordering> {
        self.as_bytes().partial_cmp(other.as_bytes())
    }
}

impl<'a> PartialEq<HeaderValue> for &'a HeaderValue {
    #[inline]
    fn eq(&self, other: &HeaderValue) -> bool {
        **self == *other
    }
}

impl<'a> PartialOrd<HeaderValue> for &'a HeaderValue {
    #[inline]
    fn partial_cmp(&self, other: &HeaderValue) -> Option<cmp::Ordering> {
        (**self).partial_cmp(other)
    }
}

impl<'a, T: ?Sized> PartialEq<&'a T> for HeaderValue
where
    HeaderValue: PartialEq<T>,
{
    #[inline]
    fn eq(&self, other: &&'a T) -> bool {
        *self == **other
    }
}

impl<'a, T: ?Sized> PartialOrd<&'a T> for HeaderValue
where
    HeaderValue: PartialOrd<T>,
{
    #[inline]
    fn partial_cmp(&self, other: &&'a T) -> Option<cmp::Ordering> {
        self.partial_cmp(*other)
    }
}

impl<'a> PartialEq<HeaderValue> for &'a str {
    #[inline]
    fn eq(&self, other: &HeaderValue) -> bool {
        *other == *self
    }
}

impl<'a> PartialOrd<HeaderValue> for &'a str {
    #[inline]
    fn partial_cmp(&self, other: &HeaderValue) -> Option<cmp::Ordering> {
        self.as_bytes().partial_cmp(other.as_bytes())
    }
}

macro_rules! from_integers {
    ($($name:ident: $t:ident => $max_len:expr),*) => {$(
        impl From<$t> for HeaderValue {
            fn from(num: $t) -> HeaderValue {
                let mut b = itoa::Buffer::new();
                let inner = Bytes::copy_from_slice(b.format(num).as_ref());
                HeaderValue {
                    inner,
                    is_sensitive: false,
                }
            }
        }

        #[test]
        fn $name() {
            let n: $t = 55;
            let val = HeaderValue::from(n);
            assert_eq!(val, &n.to_string());

            let n = ::std::$t::MAX;
            let val = HeaderValue::from(n);
            assert_eq!(val, &n.to_string());
        }
    )*};
}

from_integers! {
    // integer type => maximum decimal length

    // u8 purposely left off... HeaderValue::from(b'3') could be confusing
    from_u16: u16 => 5,
    from_i16: i16 => 6,
    from_u32: u32 => 10,
    from_i32: i32 => 11,
    from_u64: u64 => 20,
    from_i64: i64 => 20
}

#[cfg(target_pointer_width = "16")]
from_integers! {
    from_usize: usize => 5,
    from_isize: isize => 6
}

#[cfg(target_pointer_width = "32")]
from_integers! {
    from_usize: usize => 10,
    from_isize: isize => 11
}

#[cfg(target_pointer_width = "64")]
from_integers! {
    from_usize: usize => 20,
    from_isize: isize => 20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::op_ref, clippy::cmp_owned, deprecated)]
    fn test_basics() {
        assert!(HeaderValue::from_str("").unwrap().is_empty());

        let hdr = HeaderValue::from_bytes(b"upgrade").unwrap();
        let hdr2 = HeaderValue::from(&hdr);
        assert_eq!(hdr, hdr2);

        let hdr3 = HeaderValue::from_maybe_shared(Bytes::from_static(b"upgrade")).unwrap();
        assert_eq!(hdr, hdr3);

        let hdr = http::header::HeaderValue::from_bytes(b"upgrade").unwrap();
        let hdr2 = HeaderValue::from(&hdr);
        assert_eq!(hdr2.as_bytes(), b"upgrade");
        assert_eq!(hdr2.as_shared(), &Bytes::from_static(b"upgrade"));
        let hdr2 = HeaderValue::from(hdr);
        assert_eq!(hdr2.as_bytes(), b"upgrade");

        let hdr = HeaderValue::try_from("upgrade".to_string()).unwrap();
        assert_eq!(hdr.as_bytes(), b"upgrade");
        let hdr = HeaderValue::try_from(&("upgrade".to_string())).unwrap();
        assert_eq!(hdr.as_bytes(), b"upgrade");
        let hdr = HeaderValue::try_from(ByteString::from("upgrade")).unwrap();
        assert_eq!(hdr.as_bytes(), b"upgrade");
        let hdr = HeaderValue::try_from(&ByteString::from("upgrade")).unwrap();
        assert_eq!(hdr.as_bytes(), b"upgrade");
        let hdr2 = HeaderValue::try_from(&ByteString::from("upgrade2")).unwrap();

        assert!(hdr == hdr);
        assert!(hdr == &hdr);
        assert!(hdr == "upgrade");
        assert!(hdr == "upgrade".to_string());
        assert!("upgrade" == hdr);
        assert!("upgrade" == &hdr);
        assert!("upgrade".to_string() == hdr);
        assert!(hdr < hdr2);
        assert!(hdr < &hdr2);
        assert!(&hdr < &hdr2);
        assert!(&hdr < "upgrade2");
        assert!(hdr < "upgrade2");
        assert!(hdr < "upgrade2".to_string());
        assert!(hdr < &b"upgrade2"[..]);
        assert!(hdr < b"upgrade2"[..]);
        assert!(hdr != &b"upgrade2"[..]);
        assert!(hdr != b"upgrade2"[..]);
        assert!("upgrade2" > hdr);
        assert!("upgrade2".to_string() > hdr);
        assert!(b"upgrade2"[..] > hdr);
        assert!("upgrade2"[..] != hdr);
    }

    #[test]
    fn test_try_from() {
        HeaderValue::try_from(vec![127]).unwrap_err();
    }

    #[test]
    fn it_converts_using_try_from() {
        assert!(HeaderValue::from_bytes(b"upgrade").is_ok());
    }

    #[test]
    fn into_http_value() {
        let hdr = HeaderValue::from_bytes(b"upgrade").unwrap();
        let _ = http::header::HeaderValue::from(&hdr);
        let _ = http::header::HeaderValue::from(hdr);
    }

    #[test]
    fn test_fmt() {
        let cases = &[
            ("hello", "\"hello\""),
            ("hello \"world\"", "\"hello \\\"world\\\"\""),
            ("\u{7FFF}hello", "\"\\xe7\\xbf\\xbfhello\""),
        ];

        for &(value, expected) in cases {
            let val = HeaderValue::from_bytes(value.as_bytes()).unwrap();
            let actual = format!("{:?}", val);
            assert_eq!(expected, actual);
        }

        let mut sensitive = HeaderValue::from_static("password");
        sensitive.set_sensitive(true);
        assert_eq!("Sensitive", format!("{:?}", sensitive));

        let s = format!("{:?}", InvalidHeaderValue { _priv: {} });
        assert_eq!(s, "InvalidHeaderValue");

        let s = format!("{}", ToStrError { _priv: {} });
        assert_eq!(s, "failed to convert header to a str");
    }
}
