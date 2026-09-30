use serde::de::{self, Deserializer, Error as DeError, Visitor};
use serde::forward_to_deserialize_any;

use crate::{ResourcePath, path::Path, path::PathIter};

macro_rules! unsupported_type {
    ($trait_fn:ident, $name:expr) => {
        fn $trait_fn<V>(self, _: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            Err(de::value::Error::custom(concat!(
                "unsupported type: ",
                $name
            )))
        }
    };
}

macro_rules! parse_single_value {
    ($trait_fn:ident, $visit_fn:ident, $tp:tt) => {
        fn $trait_fn<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            if self.path.len() != 1 {
                Err(de::value::Error::custom(
                    format!("wrong number of parameters: {} expected 1", self.path.len()).as_str(),
                ))
            } else {
                let v = self.path[0].parse().map_err(|_| {
                    de::value::Error::custom(format!(
                        "can not parse {:?} to a {}",
                        &self.path[0], $tp
                    ))
                })?;
                visitor.$visit_fn(v)
            }
        }
    };
}

#[derive(Debug)]
/// Serde deserializer for the dynamic segments of a matched [`Path`].
///
/// A struct or map is deserialized from segments by name, a tuple or
/// sequence from segments in pattern order, a single value from the only
/// segment. See [`Path::load()`].
pub struct PathDeserializer<'de, T: ResourcePath> {
    path: &'de Path<T>,
}

impl<'de, T: ResourcePath + 'de> PathDeserializer<'de, T> {
    /// Creates a deserializer for the path.
    pub fn new(path: &'de Path<T>) -> Self {
        PathDeserializer { path }
    }
}

impl<'de, T: ResourcePath + 'de> Deserializer<'de> for PathDeserializer<'de, T> {
    type Error = de::value::Error;

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_map(ParamsDeserializer {
            params: self.path.iter(),
            current: None,
        })
    }

    fn deserialize_struct<V>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        self.deserialize_unit(visitor)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if self.path.len() < len {
            Err(de::value::Error::custom(
                format!(
                    "wrong number of parameters: {} expected {}",
                    self.path.len(),
                    len
                )
                .as_str(),
            ))
        } else {
            visitor.visit_seq(ParamsSeq {
                params: self.path.iter(),
            })
        }
    }

    fn deserialize_tuple_struct<V>(
        self,
        _: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if self.path.len() < len {
            Err(de::value::Error::custom(
                format!(
                    "wrong number of parameters: {} expected {}",
                    self.path.len(),
                    len
                )
                .as_str(),
            ))
        } else {
            visitor.visit_seq(ParamsSeq {
                params: self.path.iter(),
            })
        }
    }

    fn deserialize_enum<V>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if self.path.is_empty() {
            Err(de::value::Error::custom("expected at least one parameter"))
        } else {
            visitor.visit_enum(ValueEnum {
                value: &self.path[0],
            })
        }
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        if self.path.is_empty() {
            Err(de::value::Error::custom(
                format!("wrong number of parameters: {} expected 1", self.path.len()).as_str(),
            ))
        } else {
            visitor.visit_borrowed_str(&self.path[0])
        }
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_seq(ParamsSeq {
            params: self.path.iter(),
        })
    }

    unsupported_type!(deserialize_any, "'any'");
    unsupported_type!(deserialize_bytes, "bytes");
    unsupported_type!(deserialize_option, "Option<T>");
    unsupported_type!(deserialize_identifier, "identifier");
    unsupported_type!(deserialize_ignored_any, "ignored_any");

    parse_single_value!(deserialize_bool, visit_bool, "bool");
    parse_single_value!(deserialize_i8, visit_i8, "i8");
    parse_single_value!(deserialize_i16, visit_i16, "i16");
    parse_single_value!(deserialize_i32, visit_i32, "i32");
    parse_single_value!(deserialize_i64, visit_i64, "i64");
    parse_single_value!(deserialize_u8, visit_u8, "u8");
    parse_single_value!(deserialize_u16, visit_u16, "u16");
    parse_single_value!(deserialize_u32, visit_u32, "u32");
    parse_single_value!(deserialize_u64, visit_u64, "u64");
    parse_single_value!(deserialize_f32, visit_f32, "f32");
    parse_single_value!(deserialize_f64, visit_f64, "f64");
    parse_single_value!(deserialize_string, visit_string, "String");
    parse_single_value!(deserialize_byte_buf, visit_string, "String");
    parse_single_value!(deserialize_char, visit_char, "char");
}

struct ParamsDeserializer<'de, T: ResourcePath> {
    params: PathIter<'de, T>,
    current: Option<(&'de str, &'de str)>,
}

impl<'de, T: ResourcePath> de::MapAccess<'de> for ParamsDeserializer<'de, T> {
    type Error = de::value::Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        self.current = self.params.next().map(|ref item| (item.0, item.1));
        match self.current {
            Some((key, _)) => Ok(Some(seed.deserialize(Key { key })?)),
            None => Ok(None),
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        if let Some((_, value)) = self.current.take() {
            seed.deserialize(Value { value })
        } else {
            Err(de::value::Error::custom("unexpected item"))
        }
    }
}

struct Key<'de> {
    key: &'de str,
}

impl<'de> Deserializer<'de> for Key<'de> {
    type Error = de::value::Error;

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_str(self.key)
    }

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_borrowed_str(self.key)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes
            byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map struct enum ignored_any
    }
}

macro_rules! parse_value {
    ($trait_fn:ident, $visit_fn:ident, $tp:tt) => {
        fn $trait_fn<V>(self, visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            let v = self.value.parse().map_err(|_| {
                de::value::Error::custom(format!("can not parse {:?} to a {}", self.value, $tp))
            })?;
            visitor.$visit_fn(v)
        }
    };
}

struct Value<'de> {
    value: &'de str,
}

impl<'de> Deserializer<'de> for Value<'de> {
    type Error = de::value::Error;

    parse_value!(deserialize_bool, visit_bool, "bool");
    parse_value!(deserialize_i8, visit_i8, "i8");
    parse_value!(deserialize_i16, visit_i16, "i16");
    parse_value!(deserialize_i32, visit_i32, "i32");
    parse_value!(deserialize_i64, visit_i64, "i64");
    parse_value!(deserialize_u8, visit_u8, "u8");
    parse_value!(deserialize_u16, visit_u16, "u16");
    parse_value!(deserialize_u32, visit_u32, "u32");
    parse_value!(deserialize_u64, visit_u64, "u64");
    parse_value!(deserialize_f32, visit_f32, "f32");
    parse_value!(deserialize_f64, visit_f64, "f64");
    parse_value!(deserialize_string, visit_string, "String");
    parse_value!(deserialize_byte_buf, visit_string, "String");
    parse_value!(deserialize_char, visit_char, "char");

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_borrowed_bytes(self.value.as_bytes())
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_borrowed_str(self.value)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_some(self)
    }

    fn deserialize_enum<V>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_enum(ValueEnum { value: self.value })
    }

    fn deserialize_newtype_struct<V>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_tuple<V>(self, _: usize, _: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(de::value::Error::custom("unsupported type: tuple"))
    }

    fn deserialize_struct<V>(
        self,
        _: &'static str,
        _: &'static [&'static str],
        _: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(de::value::Error::custom("unsupported type: struct"))
    }

    fn deserialize_tuple_struct<V>(
        self,
        _: &'static str,
        _: usize,
        _: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(de::value::Error::custom("unsupported type: tuple struct"))
    }

    unsupported_type!(deserialize_any, "any");
    unsupported_type!(deserialize_seq, "seq");
    unsupported_type!(deserialize_map, "map");
    unsupported_type!(deserialize_identifier, "identifier");
}

struct ParamsSeq<'de, T: ResourcePath> {
    params: PathIter<'de, T>,
}

impl<'de, T: ResourcePath> de::SeqAccess<'de> for ParamsSeq<'de, T> {
    type Error = de::value::Error;

    fn next_element_seed<U>(&mut self, seed: U) -> Result<Option<U::Value>, Self::Error>
    where
        U: de::DeserializeSeed<'de>,
    {
        match self.params.next() {
            Some(item) => Ok(Some(seed.deserialize(Value { value: item.1 })?)),
            None => Ok(None),
        }
    }
}

struct ValueEnum<'de> {
    value: &'de str,
}

impl<'de> de::EnumAccess<'de> for ValueEnum<'de> {
    type Error = de::value::Error;
    type Variant = UnitVariant;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        Ok((seed.deserialize(Key { key: self.value })?, UnitVariant))
    }
}

struct UnitVariant;

impl<'de> de::VariantAccess<'de> for UnitVariant {
    type Error = de::value::Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        Err(de::value::Error::custom("not supported"))
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(de::value::Error::custom("not supported"))
    }

    fn struct_variant<V>(self, _: &'static [&'static str], _: V) -> Result<V::Value, Self::Error>
    where
        V: Visitor<'de>,
    {
        Err(de::value::Error::custom("not supported"))
    }
}

#[cfg(test)]
#[allow(clippy::items_after_statements)]
mod tests {
    use serde_derive::Deserialize;

    use super::*;
    use crate::path::PathItem;

    #[derive(Deserialize)]
    struct MyStruct {
        key: String,
        value: String,
    }

    #[derive(Debug, Deserialize)]
    struct Test1(String, u32);

    #[derive(Debug, Deserialize)]
    struct Test2 {
        key: String,
        value: u32,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "lowercase")]
    enum TestEnum {
        Val1,
        Val2,
    }

    #[derive(Debug, Deserialize)]
    struct Test3 {
        val: TestEnum,
    }

    #[test]
    #[allow(clippy::let_unit_value, clippy::unit_cmp)]
    fn test_request_extract() {
        let mut path = Path::new("/name/user1/");
        path.segments = vec![
            ("key", PathItem::Static("name")),
            ("value", PathItem::Static("user1")),
        ];

        let s: () = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s, ());

        let s: MyStruct = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s.key, "name");
        assert_eq!(s.value, "user1");

        let s: MyStruct = path.load().unwrap();
        assert_eq!(s.key, "name");
        assert_eq!(s.value, "user1");

        let s: (String, String) =
            de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s.0, "name");
        assert_eq!(s.1, "user1");

        let s: &str = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s, "name");

        let mut path = Path::new("/name/user1/");
        path.segments = vec![
            ("key", PathItem::Static("name")),
            ("value", PathItem::Static("32")),
        ];

        let s: Test1 = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s.0, "name");
        assert_eq!(s.1, 32);

        #[derive(Deserialize)]
        struct T(Test1);

        let s: T = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!((s.0).0, "name");
        assert_eq!((s.0).1, 32);

        let s: Test2 = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s.key, "name");
        assert_eq!(s.value, 32);

        let s: Result<(Test2,), _> = de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());

        let s: (String, u8) = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s.0, "name");
        assert_eq!(s.1, 32);

        let s: (&str, ()) = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s.0, "name");
        assert_eq!(s.1, ());

        let s: (&str, Option<u8>) =
            de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(s.0, "name");
        assert_eq!(s.1, Some(32));

        let res: Vec<String> = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(res[0], "name".to_owned());
        assert_eq!(res[1], "32".to_owned());

        #[derive(Debug, Deserialize)]
        struct S2(());
        let s: Result<S2, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_ok());

        let s: Result<(), de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_ok());

        let s: Result<(String, ()), de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_ok());
    }

    #[test]
    fn test_extract_path_single() {
        let mut path = Path::new("/name/user1/");
        path.segments = vec![("value", PathItem::Static("32"))];
        let i: i8 = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(i, 32);

        let i: (i8,) = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(i, (32,));

        let i: Result<(i8, i8), _> = de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(i.is_err());

        #[derive(Deserialize)]
        struct Test(i8);
        let i: Test = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(i.0, 32);

        path.segments.push(("value2", PathItem::Static("32")));
        let i: Result<i8, _> = de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(i.is_err());
    }

    #[test]
    fn test_extract_enum() {
        let mut path = Path::new("/val1/");
        path.segments = vec![("val", PathItem::Static("val1"))];
        let i: TestEnum = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(i, TestEnum::Val1);

        let mut path = Path::new("/val1/");
        path.segments = vec![
            ("val1", PathItem::Static("val1")),
            ("val2", PathItem::Static("val2")),
        ];
        let i: (TestEnum, TestEnum) =
            de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(i, (TestEnum::Val1, TestEnum::Val2));
    }

    #[test]
    fn test_extract_enum_value() {
        let mut path = Path::new("/val1/");
        path.segments = vec![("val", PathItem::Static("val1"))];
        let i: Test3 = de::Deserialize::deserialize(PathDeserializer::new(&path)).unwrap();
        assert_eq!(i.val, TestEnum::Val1);

        let mut path = Path::new("/val3/");
        path.segments = vec![("val", PathItem::Static("val3"))];
        let i: Result<Test3, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(i.is_err());
        assert!(format!("{i:?}").contains("unknown variant"));
    }

    #[test]
    fn test_extract_errors() {
        let mut path = Path::new("/name/");
        path.segments = vec![("value", PathItem::Static("name"))];

        let s: Result<Test1, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());
        assert!(format!("{s:?}").contains("wrong number of parameters"));

        let s: Result<Test2, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());
        assert!(format!("{s:?}").contains("can not parse"));

        let s: Result<(String, String), de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());
        assert!(format!("{s:?}").contains("wrong number of parameters"));

        let s: Result<u32, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());
        assert!(format!("{s:?}").contains("can not parse"));

        #[derive(Debug, Deserialize)]
        struct S {
            _inner: (String,),
        }
        let s: Result<S, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());
        assert!(format!("{s:?}").contains("missing field `_inner`"));

        let path = Path::new("");
        let s: Result<&str, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());
        assert!(format!("{s:?}").contains("wrong number of parameters: 0 expected 1"));

        let s: Result<TestEnum, de::value::Error> =
            de::Deserialize::deserialize(PathDeserializer::new(&path));
        assert!(s.is_err());
        assert!(format!("{s:?}").contains("expected at least one parameter"));
    }

    #[test]
    fn test_extract_value_types() {
        use std::collections::HashMap;

        #[derive(Debug, Deserialize, PartialEq)]
        struct Unit;

        #[derive(Debug, Deserialize, PartialEq)]
        struct NewType(u16);

        #[derive(Debug, Deserialize, PartialEq)]
        struct Values {
            b: bool,
            i1: i16,
            i2: i32,
            i3: i64,
            u1: u8,
            u2: u16,
            u3: u64,
            f1: f32,
            f2: f64,
            c: char,
            unit: Unit,
            nt: NewType,
            opt: Option<u32>,
            #[serde(with = "serde_bytes_str")]
            bytes: Vec<u8>,
        }

        mod serde_bytes_str {
            pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
                d: D,
            ) -> Result<Vec<u8>, D::Error> {
                struct V;
                impl serde::de::Visitor<'_> for V {
                    type Value = Vec<u8>;
                    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str("bytes")
                    }
                    fn visit_bytes<E>(self, v: &[u8]) -> Result<Vec<u8>, E> {
                        Ok(v.to_vec())
                    }
                }
                d.deserialize_bytes(V)
            }
        }

        let mut path = Path::new("/");
        path.segments = vec![
            ("b", PathItem::Static("true")),
            ("i1", PathItem::Static("-1")),
            ("i2", PathItem::Static("-2")),
            ("i3", PathItem::Static("-3")),
            ("u1", PathItem::Static("1")),
            ("u2", PathItem::Static("2")),
            ("u3", PathItem::Static("3")),
            ("f1", PathItem::Static("1.5")),
            ("f2", PathItem::Static("2.5")),
            ("c", PathItem::Static("x")),
            ("unit", PathItem::Static("")),
            ("nt", PathItem::Static("7")),
            ("opt", PathItem::Static("8")),
            ("bytes", PathItem::Static("raw")),
            ("ignored", PathItem::Static("i")),
        ];
        let v: Values = path.load().unwrap();
        assert_eq!(
            v,
            Values {
                b: true,
                i1: -1,
                i2: -2,
                i3: -3,
                u1: 1,
                u2: 2,
                u3: 3,
                f1: 1.5,
                f2: 2.5,
                c: 'x',
                unit: Unit,
                nt: NewType(7),
                opt: Some(8),
                bytes: b"raw".to_vec(),
            }
        );

        let m: HashMap<String, String> = path.load().unwrap();
        assert_eq!(m.len(), 15);
        assert_eq!(m["c"], "x");
        let mut p = Path::new("/");
        p.segments = vec![("a", PathItem::Static("1"))];
        let m: HashMap<&str, u8> = p.load().unwrap();
        assert_eq!(m["a"], 1);

        let res: Result<HashMap<u32, String>, _> = path.load();
        assert!(format!("{res:?}").contains("invalid type"), "{res:?}");

        #[derive(Debug, Deserialize)]
        struct I32 {
            _v: i32,
        }
        let mut path = Path::new("/");
        path.segments = vec![("_v", PathItem::Static("x"))];
        let res: Result<I32, _> = path.load();
        assert!(
            format!("{res:?}").contains("can not parse \\\"x\\\" to a i32"),
            "{res:?}"
        );
    }

    #[test]
    fn test_extract_single_value_types() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Unit;

        let mut path = Path::new("/");
        path.segments = vec![("v", PathItem::Static("1"))];

        let mut bool_path = Path::new("/");
        bool_path.segments = vec![("v", PathItem::Static("false"))];
        assert!(!bool_path.load::<bool>().unwrap());
        assert_eq!(path.load::<i16>().unwrap(), 1);
        assert_eq!(path.load::<i32>().unwrap(), 1);
        assert_eq!(path.load::<i64>().unwrap(), 1);
        assert_eq!(path.load::<u8>().unwrap(), 1);
        assert_eq!(path.load::<u16>().unwrap(), 1);
        assert_eq!(path.load::<u64>().unwrap(), 1);
        assert!((path.load::<f32>().unwrap() - 1.0).abs() < f32::EPSILON);
        assert!((path.load::<f64>().unwrap() - 1.0).abs() < f64::EPSILON);
        assert_eq!(path.load::<char>().unwrap(), '1');
        assert_eq!(path.load::<String>().unwrap(), "1");
        assert_eq!(path.load::<Unit>().unwrap(), Unit);

        let err = |res: Result<(), de::value::Error>| res.unwrap_err().to_string();
        assert_eq!(
            err(path.load::<Option<u8>>().map(drop)),
            "unsupported type: Option<T>"
        );
        assert_eq!(
            err(path.load::<serde::de::IgnoredAny>().map(drop)),
            "unsupported type: ignored_any"
        );
        assert_eq!(
            err(path.load::<Vec<Vec<u8>>>().map(drop)),
            "unsupported type: seq"
        );
        assert_eq!(
            err(path
                .load::<Vec<std::collections::HashMap<String, String>>>()
                .map(drop)),
            "unsupported type: map"
        );
        assert_eq!(
            err(path.load::<Vec<(u8, u8)>>().map(drop)),
            "unsupported type: tuple"
        );

        #[derive(Debug, Deserialize)]
        struct S {
            _a: u8,
        }
        #[derive(Debug, Deserialize)]
        struct TS(#[allow(dead_code)] u8, #[allow(dead_code)] u8);
        assert_eq!(
            err(path.load::<Vec<S>>().map(drop)),
            "unsupported type: struct"
        );
        assert_eq!(
            err(path.load::<Vec<TS>>().map(drop)),
            "unsupported type: tuple struct"
        );
        assert_eq!(
            err(path.load::<Vec<serde_value::Any>>().map(drop)),
            "unsupported type: any"
        );
        assert_eq!(
            err(path.load::<Vec<serde_value::Ident>>().map(drop)),
            "unsupported type: identifier"
        );
        assert_eq!(
            err(path.load::<serde_value::Any>().map(drop)),
            "unsupported type: 'any'"
        );
        assert_eq!(
            err(path.load::<serde_value::Ident>().map(drop)),
            "unsupported type: identifier"
        );
        assert_eq!(
            err(path.load::<serde_value::Bytes>().map(drop)),
            "unsupported type: bytes"
        );
    }

    /// Types that request specific deserializer methods
    mod serde_value {
        use serde::de::{Deserialize, Deserializer, IgnoredAny, Visitor};
        use std::fmt;

        struct V;
        impl Visitor<'_> for V {
            type Value = ();
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("anything")
            }
        }

        #[derive(Debug)]
        pub(super) struct Any;
        impl<'de> Deserialize<'de> for Any {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                d.deserialize_any(IgnoredAny).map(|_| Any)
            }
        }

        #[derive(Debug)]
        pub(super) struct Ident;
        impl<'de> Deserialize<'de> for Ident {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                d.deserialize_identifier(V).map(|()| Ident)
            }
        }

        #[derive(Debug)]
        pub(super) struct Bytes;
        impl<'de> Deserialize<'de> for Bytes {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                d.deserialize_bytes(V).map(|()| Bytes)
            }
        }
    }

    #[test]
    fn test_extract_enum_variants() {
        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "lowercase")]
        #[allow(dead_code)]
        enum E {
            Unit,
            New(u8),
            Tuple(u8, u8),
            Struct { a: u8 },
        }

        let mut path = Path::new("/");
        for (name, ok) in [
            ("unit", true),
            ("new", false),
            ("tuple", false),
            ("struct", false),
        ] {
            path.segments = vec![("v", PathItem::Static(name))];
            let res: Result<E, _> = path.load();
            assert_eq!(res.is_ok(), ok, "{name}");
            if !ok {
                assert!(format!("{res:?}").contains("not supported"), "{name}");
            }
            // enum as a value of a sequence element
            let res: Result<(E,), _> = path.load();
            assert_eq!(res.is_ok(), ok, "{name}");
        }
    }
}
