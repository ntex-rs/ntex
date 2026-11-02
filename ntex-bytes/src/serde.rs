use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{cmp, fmt};

use super::Bytes;

impl Serialize for Bytes {
    #[inline]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(self)
    }
}

struct BytesVisitor;

impl<'de> de::Visitor<'de> for BytesVisitor {
    type Value = Bytes;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("byte array")
    }

    #[inline]
    fn visit_seq<V>(self, mut seq: V) -> Result<Self::Value, V::Error>
    where
        V: de::SeqAccess<'de>,
    {
        let len = cmp::min(seq.size_hint().unwrap_or(0), 4096);
        let mut values = Vec::with_capacity(len);

        while let Some(value) = seq.next_element()? {
            values.push(value);
        }

        Ok(values.into())
    }

    #[inline]
    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Bytes::copy_from_slice(v))
    }

    #[inline]
    fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Bytes::from(v))
    }

    #[inline]
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Bytes::copy_from_slice(v.as_bytes()))
    }

    #[inline]
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Bytes::from(v))
    }
}

impl<'de> Deserialize<'de> for Bytes {
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Bytes, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_byte_buf(BytesVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// STATEMENT: JSON string deserialization produces matching `Bytes` contents.
    ///
    /// HOW:
    /// - Parse the JSON string `"nice bytes"` as `Bytes` and compare it with `nice bytes`.
    #[test]
    fn test_serialize() {
        let s: Bytes = serde_json::from_str(r#""nice bytes""#).unwrap();
        assert_eq!(s, "nice bytes");
        //let s: BytesMut = serde_json::from_str(r#""nice bytes""#).unwrap();
        //assert_eq!(s, "nice bytes");
    }

    /// STATEMENT: `Bytes` serializes to a JSON array of numeric byte values.
    ///
    /// HOW:
    /// - Serialize static `nice bytes` and compare the exact decimal byte array string.
    #[test]
    fn test_deserialize() {
        let s = serde_json::to_string(&Bytes::from_static(b"nice bytes")).unwrap();
        assert_eq!(s, "[110,105,99,101,32,98,121,116,101,115]");
        //let s = serde_json::to_string(&BytesMut::copy_from_slice(b"nice bytes")).unwrap();
        //assert_eq!(s, "[110,105,99,101,32,98,121,116,101,115]");
    }

    /// STATEMENT: `Bytes` deserialization accepts byte buffers, strings, and byte sequences but rejects booleans.
    ///
    /// HOW:
    /// - Deserialize `ab` from byte-buffer, borrowed and owned string, and known- or unknown-length sequence tokens.
    /// - Check a boolean token reports the expected byte-array type error.
    #[test]
    fn test_de_tokens() {
        use serde_test::{Token, assert_de_tokens, assert_de_tokens_error};

        let b = Bytes::from_static(b"ab");
        assert_de_tokens(&b, &[Token::ByteBuf(b"ab")]);
        assert_de_tokens(&b, &[Token::Str("ab")]);
        assert_de_tokens(&b, &[Token::String("ab")]);
        assert_de_tokens(
            &b,
            &[
                Token::Seq { len: Some(2) },
                Token::U8(b'a'),
                Token::U8(b'b'),
                Token::SeqEnd,
            ],
        );
        assert_de_tokens(
            &b,
            &[
                Token::Seq { len: None },
                Token::U8(b'a'),
                Token::U8(b'b'),
                Token::SeqEnd,
            ],
        );
        assert_de_tokens_error::<Bytes>(
            &[Token::Bool(true)],
            "invalid type: boolean `true`, expected byte array",
        );
    }

    /// STATEMENT: JSON serialization and deserialization round-trip `Bytes` contents.
    ///
    /// HOW:
    /// - Serialize static `nice bytes`, deserialize the generated JSON as `Bytes`, and compare with the original.
    #[test]
    fn test_json_roundtrip() {
        let b = Bytes::from_static(b"nice bytes");
        let s = serde_json::to_string(&b).unwrap();
        let b2: Bytes = serde_json::from_str(&s).unwrap();
        assert_eq!(b, b2);
    }
}
