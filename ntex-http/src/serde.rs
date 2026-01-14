use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, Unexpected, Visitor};
use serde::ser::{self, Serialize, SerializeMap, Serializer};

use super::{HeaderMap, HeaderName, HeaderValue, Value};

impl Serialize for HeaderMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.len()))?;
        for (name, value) in &self.inner {
            map.serialize_entry(name.as_str(), value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for HeaderMap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(HeaderMapVisitor)
    }
}

struct HeaderMapVisitor;

impl<'de> Visitor<'de> for HeaderMapVisitor {
    type Value = HeaderMap;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a header map")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut headers = HeaderMap::with_capacity(map.size_hint().unwrap_or(0));
        while let Some((key, value)) = map.next_entry::<&str, Value>()? {
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| {
                de::Error::invalid_value(Unexpected::Str(key), &"a valid header name")
            })?;
            headers.inner.insert(name, value);
        }
        Ok(headers)
    }
}

impl Serialize for Value {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Value::One(val) if serializer.is_human_readable() => val.serialize(serializer),
            // For non-human-readable formats, always serialize as a sequence
            Value::One(val) => [val].as_slice().serialize(serializer),
            Value::Multi(vec) => vec.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> Result<Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            return deserializer.deserialize_any(ValueVisitor);
        }
        deserializer.deserialize_seq(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a single header value or sequence of values")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::One(HeaderValueVisitor.visit_str(v)?))
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::One(HeaderValueVisitor.visit_string(v)?))
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::One(HeaderValueVisitor.visit_bytes(v)?))
    }

    fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::One(HeaderValueVisitor.visit_byte_buf(v)?))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut value: Option<Value> = None;
        while let Some(next_val) = seq.next_element()? {
            match value.as_mut() {
                Some(value) => value.append(next_val),
                None => value = Some(Value::One(next_val)),
            }
        }
        value.ok_or_else(|| de::Error::invalid_length(0, &"non-empty value"))
    }
}

impl Serialize for HeaderValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if serializer.is_human_readable() {
            return serializer.serialize_str(
                self.to_str()
                    .map_err(|err| ser::Error::custom(err.to_string()))?,
            );
        }
        serializer.serialize_bytes(self.as_bytes())
    }
}

impl<'de> Deserialize<'de> for HeaderValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            return deserializer.deserialize_string(HeaderValueVisitor);
        }
        deserializer.deserialize_byte_buf(HeaderValueVisitor)
    }
}

struct HeaderValueVisitor;

impl Visitor<'_> for HeaderValueVisitor {
    type Value = HeaderValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a header value")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        HeaderValue::from_str(v).map_err(|_| {
            de::Error::invalid_value(Unexpected::Str(v), &"a valid header value")
        })
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        HeaderValue::from_shared(v).map_err(|err| de::Error::custom(err.to_string()))
    }

    fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        HeaderValue::from_bytes(v).map_err(|_| {
            de::Error::invalid_value(Unexpected::Bytes(v), &"a valid header value")
        })
    }

    fn visit_byte_buf<E>(self, v: Vec<u8>) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        HeaderValue::from_shared(v).map_err(|err| de::Error::custom(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::*;

    #[test]
    fn test_serde_json() {
        let mut map = HeaderMap::new();
        map.insert(USER_AGENT, HeaderValue::from_static("hello"));
        map.append(USER_AGENT, HeaderValue::from_static("world"));
        assert_eq!(
            serde_json::to_string(&map).unwrap(),
            r#"{"user-agent":["hello","world"]}"#
        );

        // Make roundtrip
        map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        map.insert(CONTENT_LENGTH, 0.into());
        let map_json = serde_json::to_string(&map).unwrap();
        let map2 = serde_json::from_str::<HeaderMap>(&map_json).unwrap();
        assert_eq!(map, map2);

        // Try mixed case header names
        let map_uc = serde_json::from_str::<HeaderMap>(r#"{"X-Foo":"BAR"}"#).unwrap();
        assert_eq!(map_uc.get("x-foo").unwrap(), "BAR");
        assert_eq!(
            serde_json::to_string(&map_uc).unwrap(),
            r#"{"x-foo":"BAR"}"#
        );

        // Try decode empty header value
        let map_empty = serde_json::from_str::<HeaderMap>(r#"{"user-agent":[]}"#);
        assert!(map_empty.is_err());
        assert!(
            map_empty
                .unwrap_err()
                .to_string()
                .contains("invalid length 0, expected non-empty value")
        );
    }

    #[test]
    fn test_serde_bincode() {
        let mut map = HeaderMap::new();
        map.insert(USER_AGENT, HeaderValue::from_static("hello"));
        map.append(USER_AGENT, HeaderValue::from_static("world"));
        map.insert(HeaderName::from_static("x-foo"), "bar".parse().unwrap());
        let map_bin = bincode::serialize(&map).unwrap();
        let map2 = bincode::deserialize::<HeaderMap>(&map_bin).unwrap();
        assert_eq!(map, map2);
    }
}
