use ntex_bytes::ByteString;

use crate::chars::{self, QUERY};
use crate::error::InvalidUrl;
use crate::quoting::{Component, unquote_cow};

/// Percent-encoded URL query, without the leading `?`.
#[repr(transparent)]
pub struct Query(str);

str_type!(Query);
str_eq!(Query);

impl Query {
    /// Strictly validates a query: `*( pchar / "/" / "?" )`.
    pub fn new(src: &str) -> Result<&Query, InvalidUrl> {
        chars::check(src, QUERY)?;
        Ok(Query::from_str_unchecked(src))
    }

    /// Returns `true` if the query is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the decoded query. `+` is decoded as space; escapes of `&`, `=`,
    /// `+` and `;` are kept.
    pub fn decode(&self) -> ByteString {
        unquote_cow(&self.0, Component::Query).into()
    }

    /// Returns an iterator over decoded `key=value` pairs.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let url = Url::from_static("/?a=1&b=x+y&a=%32&flag");
    /// let pairs: Vec<_> = url.query().unwrap().pairs().collect();
    /// let pairs: Vec<_> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    /// assert_eq!(pairs, [("a", "1"), ("b", "x y"), ("a", "2"), ("flag", "")]);
    /// ```
    pub fn pairs(&self) -> QueryPairs<'_> {
        QueryPairs(self.0.split('&'))
    }

    /// Returns the first decoded value for `key`.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let url = Url::from_static("/?a=1&a=2&b=%C3%BC");
    /// let query = url.query().unwrap();
    /// assert_eq!(query.get("a").unwrap(), "1");
    /// assert_eq!(query.get("b").unwrap(), "ü");
    /// assert_eq!(query.get_all("a").collect::<Vec<_>>(), ["1", "2"]);
    /// assert!(!query.contains_key("c"));
    /// ```
    pub fn get(&self, key: &str) -> Option<ByteString> {
        self.get_all(key).next()
    }

    /// Returns all decoded values for `key`.
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = ByteString> + 'a {
        self.raw_values(key)
            .map(|v| unquote_cow(v, Component::QueryPart).into())
    }

    /// Returns `true` if the query contains `key`.
    pub fn contains_key(&self, key: &str) -> bool {
        self.raw_values(key).next().is_some()
    }

    fn raw_values<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        pieces(&self.0)
            .map(split_pair)
            .filter_map(move |(k, v)| (unquote_cow(k, Component::QueryPart) == key).then_some(v))
    }
}

/// Splits a raw query into non-empty `key=value` pieces.
pub(crate) fn pieces(query: &str) -> impl Iterator<Item = &str> {
    query.split('&').filter(|piece| !piece.is_empty())
}

/// Splits a raw `key=value` piece; the value is empty if there is no `=`.
pub(crate) fn split_pair(piece: &str) -> (&str, &str) {
    piece.split_once('=').unwrap_or((piece, ""))
}

/// Iterator over decoded query pairs, see [`Query::pairs`].
#[derive(Clone, Debug)]
pub struct QueryPairs<'a>(std::str::Split<'a, char>);

impl Iterator for QueryPairs<'_> {
    type Item = (ByteString, ByteString);

    fn next(&mut self) -> Option<Self::Item> {
        let (k, v) = split_pair(self.0.by_ref().find(|piece| !piece.is_empty())?);
        Some((
            unquote_cow(k, Component::QueryPart).into(),
            unquote_cow(v, Component::QueryPart).into(),
        ))
    }
}

/// Percent-encoded URL fragment, without the leading `#`.
#[repr(transparent)]
pub struct Fragment(str);

str_type!(Fragment);
str_eq!(Fragment);

impl Fragment {
    /// Strictly validates a fragment: `*( pchar / "/" / "?" )`.
    pub fn new(src: &str) -> Result<&Fragment, InvalidUrl> {
        chars::check(src, QUERY)?;
        Ok(Fragment::from_str_unchecked(src))
    }

    /// Returns the decoded fragment.
    pub fn decode(&self) -> ByteString {
        unquote_cow(&self.0, Component::Fragment).into()
    }
}
