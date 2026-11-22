//! JSON encoding and decoding backend.
//!
//! JSON bodies are encoded and decoded with `sonic-rs`, or with `serde_json`
//! when the `serde` feature is enabled.
use std::{error, fmt};

use serde::{Serialize, de::DeserializeOwned};

use crate::util::Bytes;

/// An error that occurred while encoding or decoding a JSON body.
///
/// JSON bodies are encoded and decoded with `sonic-rs`, or with `serde_json`
/// when the `serde` feature is enabled. Errors of both crates convert into
/// `JsonError`.
///
/// The `Display` text is a single line. It does not include the input
/// fragment that `sonic-rs` appends to syntax errors.
pub struct JsonError(Inner);

enum Inner {
    Serde(serde_json::Error),
    Sonic(sonic_rs::Error),
}

impl JsonError {
    /// Returns the one-based line of the input at which the error occurred.
    ///
    /// Returns `0` if the error has no input position, for example an
    /// encoding error.
    pub fn line(&self) -> usize {
        match &self.0 {
            Inner::Serde(err) => err.line(),
            Inner::Sonic(err) => err.line(),
        }
    }

    /// Returns the one-based column of the input at which the error occurred.
    ///
    /// Returns `0` if the error has no input position, for example an
    /// encoding error.
    pub fn column(&self) -> usize {
        match &self.0 {
            Inner::Serde(err) => err.column(),
            Inner::Sonic(err) => err.column(),
        }
    }

    /// Returns `true` if the error was caused by a failure to read or write
    /// bytes on an I/O stream.
    pub fn is_io(&self) -> bool {
        match &self.0 {
            Inner::Serde(err) => err.is_io(),
            Inner::Sonic(err) => err.is_io(),
        }
    }

    /// Returns `true` if the input is not syntactically valid JSON.
    pub fn is_syntax(&self) -> bool {
        match &self.0 {
            Inner::Serde(err) => err.is_syntax(),
            Inner::Sonic(err) => err.is_syntax(),
        }
    }

    /// Returns `true` if the input is valid JSON but does not match the
    /// expected type, for example a missing field or a value of another type.
    pub fn is_data(&self) -> bool {
        match &self.0 {
            Inner::Serde(err) => err.is_data(),
            Inner::Sonic(err) => err.is_unmatched_type() || err.is_not_found(),
        }
    }

    /// Returns `true` if the input ended before the JSON value was complete.
    pub fn is_eof(&self) -> bool {
        match &self.0 {
            Inner::Serde(err) => err.is_eof(),
            Inner::Sonic(err) => err.is_eof(),
        }
    }
}

impl From<serde_json::Error> for JsonError {
    fn from(err: serde_json::Error) -> Self {
        JsonError(Inner::Serde(err))
    }
}

impl From<sonic_rs::Error> for JsonError {
    fn from(err: sonic_rs::Error) -> Self {
        JsonError(Inner::Sonic(err))
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Inner::Serde(err) => fmt::Display::fmt(err, f),
            Inner::Sonic(err) => {
                // sonic-rs appends the input fragment on the following lines
                let msg = err.to_string();
                f.write_str(msg.split('\n').next().unwrap_or_default())
            }
        }
    }
}

impl fmt::Debug for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("JsonError")
            .field(&format_args!("{self}"))
            .finish()
    }
}

impl error::Error for JsonError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match &self.0 {
            Inner::Serde(err) => err.source(),
            Inner::Sonic(err) => err.source(),
        }
    }
}

/// Deserializes a `T` from a JSON body.
pub(crate) fn from_slice<T: DeserializeOwned>(body: &[u8]) -> Result<T, JsonError> {
    #[cfg(feature = "serde")]
    {
        Ok(serde_json::from_slice(body)?)
    }
    #[cfg(not(feature = "serde"))]
    {
        Ok(sonic_rs::from_slice(body)?)
    }
}

/// Serializes `value` as a JSON body.
pub(crate) fn to_bytes<T: ?Sized + Serialize>(value: &T) -> Result<Bytes, JsonError> {
    #[cfg(feature = "serde")]
    {
        let mut buf = crate::util::BytesMut::new();
        // The serializer issues many small writes, `SpareWriter` keeps the write
        // position local instead of updating the buffer header on every write.
        serde_json::to_writer(buf.spare_writer(0), value)?;
        Ok(buf.freeze())
    }
    #[cfg(not(feature = "serde"))]
    {
        Ok(Bytes::from(sonic_rs::to_vec(value)?))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Item {
        name: String,
        value: f64,
        tags: Vec<String>,
        extra: Option<BTreeMap<String, i64>>,
    }

    /// STATEMENT: The JSON body encoder produces the same bytes as `serde_json::to_string`.
    ///
    /// HOW:
    /// - Encode collections, a map, escaped and unicode strings, floats, non-finite floats,
    ///   and an empty string, and compare each body with the `serde_json` output.
    /// - Assert a small body is stored inline.
    #[test]
    fn to_bytes_matches_serde_json() {
        let small = vec![1, 2, 3];
        let large: Vec<String> = (0..1000).map(|i| format!("item-{i}")).collect();
        let map: BTreeMap<_, _> = (0..50).map(|i| (i.to_string(), i)).collect();
        let item = Item {
            name: "quote \" backslash \\ ctrl \u{1} tab \t nl \n é 😀 </script>".to_string(),
            value: 0.1 + 0.2,
            tags: vec![String::new(), "a".repeat(100)],
            extra: Some([("k".to_string(), -1), ("m".to_string(), i64::MAX)].into()),
        };
        let floats = [0.0, -0.0, 1.5, 1e300, 1e-300, 123_456_789.012_345_68, f64::MIN];

        fn check<T: ?Sized + Serialize>(value: &T) {
            assert_eq!(to_bytes(value).unwrap(), serde_json::to_string(value).unwrap());
        }
        check(&small);
        check(&large);
        check(&map);
        check(&item);
        check(&floats);
        check(&[f64::NAN, f64::INFINITY]);
        check(&(u64::MAX, i64::MIN, true, (), None::<u8>));
        check("");

        assert!(to_bytes(&small).unwrap().is_inline());
    }

    /// STATEMENT: The JSON body encoder reports values that cannot be encoded.
    ///
    /// HOW:
    /// - Encode a map with a sequence key and assert an error with no input position.
    #[test]
    fn to_bytes_error() {
        let mut map = HashMap::new();
        map.insert(vec![1u8], 1);
        let err = to_bytes(&map).unwrap_err();
        assert_eq!(err.line(), 0);
        assert!(!err.is_io());
    }

    /// STATEMENT: The JSON body decoder parses the same values as `serde_json` and rejects
    /// trailing data.
    ///
    /// HOW:
    /// - Decode a struct and a `serde_json::Value` from the encoded body and compare them.
    /// - Assert trailing characters after the value are rejected and trailing whitespace is not.
    #[test]
    fn from_slice_roundtrip() {
        let item = Item {
            name: "é \" 😀".to_string(),
            value: -2.5,
            tags: vec!["x".to_string()],
            extra: None,
        };
        let body = to_bytes(&item).unwrap();
        assert_eq!(from_slice::<Item>(&body).unwrap(), item);
        assert_eq!(
            from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        );

        assert!(from_slice::<serde_json::Value>(b"{\"a\":1} \n").is_ok());
        assert!(from_slice::<serde_json::Value>(b"{\"a\":1} x").is_err());
    }

    /// STATEMENT: The JSON body decoder rejects invalid input without panicking.
    ///
    /// HOW:
    /// - Decode invalid UTF-8 in a string, a truncated object, and a too deeply nested array,
    ///   and assert each fails with the expected classification.
    /// - Decode a value of the wrong type and a missing field and assert data errors.
    #[test]
    fn from_slice_errors() {
        let err = from_slice::<String>(b"\"\xff\xfe\"").unwrap_err();
        assert!(!err.is_data());

        let err = from_slice::<serde_json::Value>(b"{\"a\":").unwrap_err();
        assert!(err.is_eof());

        let nested = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        let err = from_slice::<serde_json::Value>(nested.as_bytes()).unwrap_err();
        assert!(err.is_syntax());

        let err = from_slice::<u32>(b"\"x\"").unwrap_err();
        assert!(err.is_data());
        let err = from_slice::<Item>(b"{\"name\":\"a\"}").unwrap_err();
        assert!(err.is_data());
    }

    /// STATEMENT: `JsonError` keeps the position of both backends and displays one line.
    ///
    /// HOW:
    /// - Convert `serde_json` and `sonic-rs` syntax errors for a multi-line input.
    /// - Assert line and column match the backend, `Display` has no newline and no input
    ///   fragment, and `Debug` wraps the same text.
    #[test]
    fn error_display() {
        let input = b"{\n  \"secret\": tru }";
        let serde = JsonError::from(serde_json::from_slice::<serde_json::Value>(input).unwrap_err());
        let sonic = JsonError::from(sonic_rs::from_slice::<serde_json::Value>(input).unwrap_err());

        for err in [serde, sonic] {
            assert!(err.is_syntax());
            assert_eq!(err.line(), 2);
            assert!(err.column() > 0);
            let msg = err.to_string();
            assert!(!msg.contains('\n'), "{msg:?}");
            assert!(!msg.contains("secret"), "{msg:?}");
            assert!(msg.contains("line 2"), "{msg:?}");
            assert_eq!(format!("{err:?}"), format!("JsonError({msg})"));
        }
    }
}
