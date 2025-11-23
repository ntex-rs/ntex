use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{cmp, fmt};

use super::{Bytes, BytesMut};

macro_rules! serde_impl {
    ($ty:ident, $visitor_ty:ident, $from_slice:ident) => {
        impl Serialize for $ty {
            #[inline]
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_bytes(&self)
            }
        }

        struct $visitor_ty;

        impl<'de> de::Visitor<'de> for $visitor_ty {
            type Value = $ty;

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
                Ok($ty::$from_slice(v))
            }

            #[inline]
            fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok($ty::from(v))
            }

            #[inline]
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok($ty::$from_slice(v.as_bytes()))
            }

            #[inline]
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok($ty::from(v))
            }
        }

        impl<'de> Deserialize<'de> for $ty {
            #[inline]
            fn deserialize<D>(deserializer: D) -> Result<$ty, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserializer.deserialize_byte_buf($visitor_ty)
            }
        }
    };
}

serde_impl!(Bytes, BytesVisitor, copy_from_slice);
serde_impl!(BytesMut, BytesMutVisitor, from);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize() {
        let s: Bytes = serde_json::from_str(r#""nice bytes""#).unwrap();
        assert_eq!(s, "nice bytes");
        let s: BytesMut = serde_json::from_str(r#""nice bytes""#).unwrap();
        assert_eq!(s, "nice bytes");
    }

    #[test]
    fn test_deserialize() {
        let s = serde_json::to_string(&Bytes::from_static(b"nice bytes")).unwrap();
        assert_eq!(s, "[110,105,99,101,32,98,121,116,101,115]");
        let s = serde_json::to_string(&BytesMut::copy_from_slice(b"nice bytes")).unwrap();
        assert_eq!(s, "[110,105,99,101,32,98,121,116,101,115]");
    }
}
