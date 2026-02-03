use std::collections::{self, VecDeque, hash_map, hash_map::Entry};
use std::fmt;

use foldhash::fast::RandomState;

use crate::{HeaderName, HeaderValue};

type HashMap<K, V> = collections::HashMap<K, V, RandomState>;

/// Combines two different futures, streams, or sinks having the same associated types into a single
/// type.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Either<A, B> {
    /// First branch of the type
    Left(A),
    /// Second branch of the type
    Right(B),
}

/// A set of HTTP headers
///
/// `HeaderMap` is an multimap of [`HeaderName`] to values.
///
/// [`HeaderName`]: struct.HeaderName.html
#[derive(Clone, PartialEq, Eq)]
pub struct HeaderMap {
    pub(crate) inner: HashMap<HeaderName, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    One(HeaderValue),
    Multi(VecDeque<HeaderValue>),
}

impl Value {
    fn get(&self) -> &HeaderValue {
        match self {
            Value::One(val) => val,
            Value::Multi(val) => &val[0],
        }
    }

    fn get_mut(&mut self) -> &mut HeaderValue {
        match self {
            Value::One(val) => val,
            Value::Multi(val) => &mut val[0],
        }
    }

    pub(crate) fn append(&mut self, val: HeaderValue) {
        match self {
            Value::One(prev_val) => {
                let prev_val = std::mem::replace(prev_val, val);
                let mut val = VecDeque::new();
                val.push_back(prev_val);
                let data = std::mem::replace(self, Value::Multi(val));
                match data {
                    Value::One(val) => self.append(val),
                    Value::Multi(_) => unreachable!(),
                }
            }
            Value::Multi(vec) => vec.push_back(val),
        }
    }
}

#[derive(Debug)]
pub struct ValueIntoIter {
    value: Value,
}

impl Iterator for ValueIntoIter {
    type Item = HeaderValue;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.value {
            Value::One(_) => {
                let val = std::mem::replace(
                    &mut self.value,
                    Value::Multi(VecDeque::with_capacity(0)),
                );
                match val {
                    Value::One(val) => Some(val),
                    Value::Multi(_) => unreachable!(),
                }
            }
            Value::Multi(vec) => vec.pop_front(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self.value {
            Value::One(_) => (1, None),
            Value::Multi(ref v) => v.iter().size_hint(),
        }
    }
}

impl IntoIterator for Value {
    type Item = HeaderValue;
    type IntoIter = ValueIntoIter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        ValueIntoIter { value: self }
    }
}

impl Extend<HeaderValue> for Value {
    #[inline]
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = HeaderValue>,
    {
        for h in iter {
            self.append(h);
        }
    }
}

impl From<HeaderValue> for Value {
    #[inline]
    fn from(hdr: HeaderValue) -> Value {
        Value::One(hdr)
    }
}

impl<'a> From<&'a HeaderValue> for Value {
    #[inline]
    fn from(hdr: &'a HeaderValue) -> Value {
        Value::One(hdr.clone())
    }
}

impl Default for HeaderMap {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl HeaderMap {
    /// Create an empty `HeaderMap`.
    ///
    /// The map will be created without any capacity. This function will not
    /// allocate.
    pub fn new() -> Self {
        HeaderMap {
            inner: HashMap::default(),
        }
    }

    /// Create an empty `HeaderMap` with the specified capacity.
    ///
    /// The returned map will allocate internal storage in order to hold about
    /// `capacity` elements without reallocating. However, this is a "best
    /// effort" as there are usage patterns that could cause additional
    /// allocations before `capacity` headers are stored in the map.
    ///
    /// More capacity than requested may be allocated.
    pub fn with_capacity(capacity: usize) -> HeaderMap {
        HeaderMap {
            inner: HashMap::with_capacity_and_hasher(capacity, RandomState::default()),
        }
    }

    /// Returns the number of keys stored in the map.
    ///
    /// This number could be be less than or equal to actual headers stored in
    /// the map.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true if the map contains no elements.
    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    /// Clears the map, removing all key-value pairs. Keeps the allocated memory
    /// for reuse.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Returns the number of headers the map can hold without reallocating.
    ///
    /// This number is an approximation as certain usage patterns could cause
    /// additional allocations before the returned capacity is filled.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Reserves capacity for at least `additional` more headers to be inserted
    /// into the `HeaderMap`.
    ///
    /// The header map may reserve more space to avoid frequent reallocations.
    /// Like with `with_capacity`, this will be a "best effort" to avoid
    /// allocations until `additional` more headers are inserted. Certain usage
    /// patterns could cause additional allocations before the number is
    /// reached.
    pub fn reserve(&mut self, additional: usize) {
        self.inner.reserve(additional);
    }

    /// Returns a reference to the value associated with the key.
    ///
    /// If there are multiple values associated with the key, then the first one
    /// is returned. Use `get_all` to get all values associated with a given
    /// key. Returns `None` if there are no values associated with the key.
    pub fn get<N: AsName>(&self, name: N) -> Option<&HeaderValue> {
        self.get2(name).map(Value::get)
    }

    fn get2<N: AsName>(&self, name: N) -> Option<&Value> {
        match name.as_name() {
            Either::Left(name) => self.inner.get(name),
            Either::Right(s) => {
                if let Ok(name) = HeaderName::try_from(s) {
                    self.inner.get(&name)
                } else {
                    None
                }
            }
        }
    }

    /// Returns a view of all values associated with a key.
    ///
    /// The returned view does not incur any allocations and allows iterating
    /// the values associated with the key.  See [`GetAll`] for more details.
    /// Returns `None` if there are no values associated with the key.
    ///
    /// [`GetAll`]: struct.GetAll.html
    pub fn get_all<N: AsName>(&self, name: N) -> GetAll<'_> {
        GetAll {
            idx: 0,
            item: self.get2(name),
        }
    }

    /// Returns a mutable reference to the value associated with the key.
    ///
    /// If there are multiple values associated with the key, then the first one
    /// is returned. Use `entry` to get all values associated with a given
    /// key. Returns `None` if there are no values associated with the key.
    pub fn get_mut<N: AsName>(&mut self, name: N) -> Option<&mut HeaderValue> {
        match name.as_name() {
            Either::Left(name) => self.inner.get_mut(name).map(Value::get_mut),
            Either::Right(s) => {
                if let Ok(name) = HeaderName::try_from(s) {
                    self.inner.get_mut(&name).map(Value::get_mut)
                } else {
                    None
                }
            }
        }
    }

    /// Returns true if the map contains a value for the specified key.
    pub fn contains_key<N: AsName>(&self, key: N) -> bool {
        match key.as_name() {
            Either::Left(name) => self.inner.contains_key(name),
            Either::Right(s) => {
                if let Ok(name) = HeaderName::try_from(s) {
                    self.inner.contains_key(&name)
                } else {
                    false
                }
            }
        }
    }

    /// An iterator visiting all key-value pairs.
    ///
    /// The iteration order is arbitrary, but consistent across platforms for
    /// the same crate version. Each key will be yielded once per associated
    /// value. So, if a key has 3 associated values, it will be yielded 3 times.
    pub fn iter(&self) -> Iter<'_> {
        Iter::new(self.inner.iter())
    }

    #[doc(hidden)]
    pub fn iter_inner(&self) -> hash_map::Iter<'_, HeaderName, Value> {
        self.inner.iter()
    }

    /// An iterator visiting all keys.
    ///
    /// The iteration order is arbitrary, but consistent across platforms for
    /// the same crate version. Each key will be yielded only once even if it
    /// has multiple associated values.
    pub fn keys(&self) -> Keys<'_> {
        Keys(self.inner.keys())
    }

    /// Inserts a key-value pair into the map.
    ///
    /// If the map did not previously have this key present, then `None` is
    /// returned.
    ///
    /// If the map did have this key present, the new value is associated with
    /// the key and all previous values are removed. **Note** that only a single
    /// one of the previous values is returned. If there are multiple values
    /// that have been previously associated with the key, then the first one is
    /// returned. See `insert_mult` on `OccupiedEntry` for an API that returns
    /// all values.
    ///
    /// The key is not updated, though; this matters for types that can be `==`
    /// without being identical.
    pub fn insert(&mut self, key: HeaderName, val: HeaderValue) {
        let _ = self.inner.insert(key, Value::One(val));
    }

    /// Inserts a key-value pair into the map.
    ///
    /// If the map did not previously have this key present, then `false` is
    /// returned.
    ///
    /// If the map did have this key present, the new value is pushed to the end
    /// of the list of values currently associated with the key. The key is not
    /// updated, though; this matters for types that can be `==` without being
    /// identical.
    pub fn append(&mut self, key: HeaderName, value: HeaderValue) {
        match self.inner.entry(key) {
            Entry::Occupied(mut entry) => entry.get_mut().append(value),
            Entry::Vacant(entry) => {
                entry.insert(Value::One(value));
            }
        }
    }

    /// Removes all headers for a particular header name from the map.
    pub fn remove<N: AsName>(&mut self, key: N) {
        match key.as_name() {
            Either::Left(name) => {
                let _ = self.inner.remove(name);
            }
            Either::Right(s) => {
                if let Ok(name) = HeaderName::try_from(s) {
                    let _ = self.inner.remove(&name);
                }
            }
        }
    }
}

#[doc(hidden)]
pub trait AsName {
    fn as_name(&self) -> Either<&HeaderName, &str>;
}

impl AsName for HeaderName {
    fn as_name(&self) -> Either<&HeaderName, &str> {
        Either::Left(self)
    }
}

impl AsName for &HeaderName {
    fn as_name(&self) -> Either<&HeaderName, &str> {
        Either::Left(self)
    }
}

impl AsName for &str {
    fn as_name(&self) -> Either<&HeaderName, &str> {
        Either::Right(self)
    }
}

impl AsName for String {
    fn as_name(&self) -> Either<&HeaderName, &str> {
        Either::Right(self.as_str())
    }
}

impl AsName for &String {
    fn as_name(&self) -> Either<&HeaderName, &str> {
        Either::Right(self.as_str())
    }
}

impl<N: std::fmt::Display, V> FromIterator<(N, V)> for HeaderMap
where
    HeaderName: TryFrom<N>,
    Value: TryFrom<V>,
    V: std::fmt::Debug,
{
    #[inline]
    #[allow(clippy::mutable_key_type)]
    fn from_iter<T: IntoIterator<Item = (N, V)>>(iter: T) -> Self {
        let map = iter
            .into_iter()
            .filter_map(|(n, v)| {
                let name = format!("{n}");
                match (HeaderName::try_from(n), Value::try_from(v)) {
                    (Ok(n), Ok(v)) => Some((n, v)),
                    (Ok(n), Err(_)) => {
                        log::warn!("failed to parse `{n}` header value");
                        None
                    }
                    (Err(_), Ok(_)) => {
                        log::warn!("invalid HTTP header name: {name}");
                        None
                    }
                    (Err(_), Err(_)) => {
                        log::warn!("invalid HTTP header name `{name}` and value");
                        None
                    }
                }
            })
            .fold(HashMap::default(), |mut map: HashMap<_, Value>, (n, v)| {
                match map.entry(n) {
                    Entry::Occupied(mut oc) => oc.get_mut().extend(v),
                    Entry::Vacant(va) => {
                        let _ = va.insert(v);
                    }
                }
                map
            });
        HeaderMap { inner: map }
    }
}

impl FromIterator<HeaderValue> for Value {
    fn from_iter<T: IntoIterator<Item = HeaderValue>>(iter: T) -> Self {
        let mut iter = iter.into_iter();
        let value = iter.next().map(Value::One);
        let mut value = match value {
            Some(v) => v,
            _ => Value::One(HeaderValue::from_static("")),
        };
        value.extend(iter);
        value
    }
}

impl TryFrom<&str> for Value {
    type Error = crate::header::InvalidHeaderValue;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Ok(value
            .split(',')
            .filter(|v| !v.is_empty())
            .map(str::trim)
            .filter_map(|v| HeaderValue::from_str(v).ok())
            .collect::<Value>())
    }
}

#[derive(Debug)]
pub struct GetAll<'a> {
    idx: usize,
    item: Option<&'a Value>,
}

impl<'a> Iterator for GetAll<'a> {
    type Item = &'a HeaderValue;

    #[inline]
    fn next(&mut self) -> Option<&'a HeaderValue> {
        if let Some(ref val) = self.item {
            match val {
                Value::One(val) => {
                    self.item.take();
                    Some(val)
                }
                Value::Multi(vec) => {
                    if self.idx < vec.len() {
                        let item = Some(&vec[self.idx]);
                        self.idx += 1;
                        item
                    } else {
                        self.item.take();
                        None
                    }
                }
            }
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct Keys<'a>(hash_map::Keys<'a, HeaderName, Value>);

impl<'a> Iterator for Keys<'a> {
    type Item = &'a HeaderName;

    #[inline]
    fn next(&mut self) -> Option<&'a HeaderName> {
        self.0.next()
    }
}

impl<'a> IntoIterator for &'a HeaderMap {
    type Item = (&'a HeaderName, &'a HeaderValue);
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[derive(Debug)]
pub struct Iter<'a> {
    idx: usize,
    current: Option<(&'a HeaderName, &'a VecDeque<HeaderValue>)>,
    iter: hash_map::Iter<'a, HeaderName, Value>,
}

impl<'a> Iter<'a> {
    fn new(iter: hash_map::Iter<'a, HeaderName, Value>) -> Self {
        Self {
            iter,
            idx: 0,
            current: None,
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a HeaderName, &'a HeaderValue);

    #[inline]
    fn next(&mut self) -> Option<(&'a HeaderName, &'a HeaderValue)> {
        if let Some(ref mut item) = self.current {
            if self.idx < item.1.len() {
                let item = (item.0, &item.1[self.idx]);
                self.idx += 1;
                return Some(item);
            }
            self.idx = 0;
            self.current.take();
        }
        if let Some(item) = self.iter.next() {
            match item.1 {
                Value::One(value) => Some((item.0, value)),
                Value::Multi(vec) => {
                    self.current = Some((item.0, vec));
                    self.next()
                }
            }
        } else {
            None
        }
    }
}

impl fmt::Debug for HeaderMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_map();

        for (key, val) in &self.inner {
            match val {
                Value::One(val) => {
                    let _ = f.entry(&key, &val);
                }
                Value::Multi(val) => {
                    for v in val {
                        f.entry(&key, &v);
                    }
                }
            }
        }
        f.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{ACCEPT_ENCODING, CONTENT_TYPE};

    #[test]
    fn test_from_iter() {
        let vec = vec![
            ("Connection", "keep-alive"),
            ("Accept", "text/html"),
            (
                "Accept",
                "*/*, application/xhtml+xml, application/xml;q=0.9, image/webp,",
            ),
        ];
        let map = HeaderMap::from_iter(vec);
        assert_eq!(
            map.get("Connection"),
            Some(&HeaderValue::from_static("keep-alive"))
        );
        assert_eq!(
            map.get_all("Accept").collect::<Vec<&HeaderValue>>(),
            vec![
                &HeaderValue::from_static("text/html"),
                &HeaderValue::from_static("*/*"),
                &HeaderValue::from_static("application/xhtml+xml"),
                &HeaderValue::from_static("application/xml;q=0.9"),
                &HeaderValue::from_static("image/webp"),
            ]
        )
    }

    #[test]
    #[allow(clippy::needless_borrow, clippy::needless_borrows_for_generic_args)]
    fn test_basics() {
        let m = HeaderMap::default();
        assert!(m.is_empty());
        let mut m = HeaderMap::with_capacity(10);
        assert!(m.is_empty());
        assert!(m.capacity() >= 10);
        m.reserve(20);
        assert!(m.capacity() >= 20);

        m.insert(CONTENT_TYPE, HeaderValue::from_static("text"));
        assert!(m.contains_key(CONTENT_TYPE));
        assert!(m.contains_key("content-type"));
        assert!(m.contains_key("content-type".to_string()));
        assert!(m.contains_key(&("content-type".to_string())));
        assert_eq!(
            *m.get_mut("content-type").unwrap(),
            HeaderValue::from_static("text")
        );
        assert_eq!(
            *m.get_mut(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text")
        );
        assert!(format!("{m:?}").contains("content-type"));

        assert!(m.keys().any(|x| x == CONTENT_TYPE));
        m.remove("content-type");
        assert!(m.is_empty());
    }

    #[test]
    fn test_append() {
        let mut map = HeaderMap::new();

        map.append(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        assert_eq!(
            map.get_all(ACCEPT_ENCODING).collect::<Vec<_>>(),
            vec![&HeaderValue::from_static("gzip"),]
        );

        map.append(ACCEPT_ENCODING, HeaderValue::from_static("br"));
        map.append(ACCEPT_ENCODING, HeaderValue::from_static("deflate"));
        assert_eq!(
            map.get_all(ACCEPT_ENCODING).collect::<Vec<_>>(),
            vec![
                &HeaderValue::from_static("gzip"),
                &HeaderValue::from_static("br"),
                &HeaderValue::from_static("deflate"),
            ]
        );
        assert_eq!(
            map.get(ACCEPT_ENCODING),
            Some(&HeaderValue::from_static("gzip"))
        );
        assert_eq!(
            map.get_mut(ACCEPT_ENCODING),
            Some(&mut HeaderValue::from_static("gzip"))
        );

        map.remove(ACCEPT_ENCODING);
        assert_eq!(map.get(ACCEPT_ENCODING), None);
    }

    #[test]
    fn test_from_http() {
        let mut map = http::HeaderMap::new();
        map.append(ACCEPT_ENCODING, http::HeaderValue::from_static("gzip"));

        let map2 = HeaderMap::from(map);
        assert_eq!(
            map2.get(ACCEPT_ENCODING),
            Some(&HeaderValue::from_static("gzip"))
        );
    }
}
