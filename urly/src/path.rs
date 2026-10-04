use ntex_bytes::ByteString;

use crate::chars::{self, PATH, split_at_char};
use crate::error::InvalidUrl;
use crate::query::Query;
use crate::quoting::{Component, unquote, unquote_path_safe};

/// Percent-encoded URL path.
#[repr(transparent)]
pub struct Path(str);

str_type!(Path);
str_eq!(Path);

impl Path {
    /// Strictly validates a path: `*( pchar / "/" )`.
    pub fn new(src: &str) -> Result<&Path, InvalidUrl> {
        chars::check(src, PATH)?;
        Ok(Path::from_str_unchecked(src))
    }

    /// Converts a static string to a path.
    ///
    /// # Panics
    ///
    /// Panics if the path is not valid.
    pub fn from_static(src: &'static str) -> &'static Path {
        match Path::new(src) {
            Ok(path) => path,
            Err(e) => panic!("invalid static path {src:?}: {e}"),
        }
    }

    /// Returns `true` if the path is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns `true` if the path starts with `/`.
    pub fn is_absolute(&self) -> bool {
        self.0.starts_with('/')
    }

    /// Returns the decoded path.
    ///
    /// ```
    /// use urly::Path;
    ///
    /// assert_eq!(Path::from_static("/a%20b/c%2Fd").decode(), "/a b/c/d");
    /// ```
    pub fn decode(&self) -> ByteString {
        unquote(&self.0, Component::Path).into()
    }

    /// Returns the decoded path, keeping `%2F` and `%25` encoded, so the result
    /// can still be split into segments.
    ///
    /// ```
    /// use urly::Path;
    ///
    /// assert_eq!(Path::from_static("/a%20b/c%2Fd").decode_safe(), "/a b/c%2Fd");
    /// ```
    pub fn decode_safe(&self) -> ByteString {
        unquote_path_safe(&self.0).into()
    }

    /// Returns an iterator over the percent-encoded segments.
    ///
    /// ```
    /// use urly::Path;
    ///
    /// let segments: Vec<_> = Path::from_static("/a/b%20c/").segments().collect();
    /// assert_eq!(segments, ["a", "b%20c", ""]);
    /// assert_eq!(Path::from_static("").segments().count(), 0);
    /// ```
    pub fn segments(&self) -> Segments<'_> {
        let path = self.0.strip_prefix('/').unwrap_or(&self.0);
        Segments((!self.0.is_empty()).then(|| path.split('/')))
    }

    /// Returns the last segment, unless it is empty, `.` or `..`.
    ///
    /// ```
    /// use urly::Path;
    ///
    /// assert_eq!(Path::from_static("/a/file.tar.gz").file_name(), Some("file.tar.gz"));
    /// assert_eq!(Path::from_static("/a/").file_name(), None);
    /// ```
    pub fn file_name(&self) -> Option<&str> {
        self.0
            .rsplit('/')
            .next()
            .filter(|name| !matches!(*name, "" | "." | ".."))
    }

    /// Returns the file name without its last extension.
    ///
    /// ```
    /// use urly::Path;
    ///
    /// assert_eq!(Path::from_static("/file.tar.gz").file_stem(), Some("file.tar"));
    /// assert_eq!(Path::from_static("/.hidden").file_stem(), Some(".hidden"));
    /// ```
    pub fn file_stem(&self) -> Option<&str> {
        self.file_name().map(|name| split_extension(name).0)
    }

    /// Returns the last extension of the file name, without the dot.
    ///
    /// ```
    /// use urly::Path;
    ///
    /// assert_eq!(Path::from_static("/file.tar.gz").extension(), Some("gz"));
    /// assert_eq!(Path::from_static("/.hidden").extension(), None);
    /// ```
    pub fn extension(&self) -> Option<&str> {
        self.file_name().and_then(|name| split_extension(name).1)
    }

    /// Returns all extensions of the file name, without the leading dot.
    ///
    /// ```
    /// use urly::Path;
    ///
    /// assert_eq!(Path::from_static("/file.tar.gz").extensions(), Some("tar.gz"));
    /// assert_eq!(Path::from_static("/file").extensions(), None);
    /// ```
    pub fn extensions(&self) -> Option<&str> {
        let name = self.file_name()?;
        let start = usize::from(name.starts_with('.'));
        name[start..].find('.').map(|i| &name[start + i + 1..])
    }
}

fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        None | Some(0) => (name, None),
        Some(i) => (&name[..i], Some(&name[i + 1..])),
    }
}

/// Iterator over percent-encoded path segments, see [`Path::segments`].
#[derive(Clone, Debug)]
pub struct Segments<'a>(Option<std::str::Split<'a, char>>);

impl<'a> Iterator for Segments<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        self.0.as_mut()?.next()
    }
}

impl DoubleEndedIterator for Segments<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.as_mut()?.next_back()
    }
}

/// Path with an optional query: `path ["?" query]`.
#[repr(transparent)]
pub struct PathAndQuery(str);

str_type!(PathAndQuery);
str_eq!(PathAndQuery);

impl PathAndQuery {
    /// Strictly validates a path and query.
    ///
    /// ```
    /// use urly::PathAndQuery;
    ///
    /// let pq = PathAndQuery::new("/search?q=rust").unwrap();
    /// assert_eq!(pq.path(), "/search");
    /// assert_eq!(pq.query().unwrap(), "q=rust");
    /// ```
    pub fn new(src: &str) -> Result<&PathAndQuery, InvalidUrl> {
        let (path, query) = split_at_char(src, '?');
        Path::new(path)?;
        if let Some(query) = query {
            Query::new(query).map_err(|e| e.offset(path.len() + 1))?;
        }
        Ok(PathAndQuery::from_str_unchecked(src))
    }

    /// Converts a static string to a path and query.
    ///
    /// # Panics
    ///
    /// Panics if the path or query is not valid.
    pub fn from_static(src: &'static str) -> &'static PathAndQuery {
        match PathAndQuery::new(src) {
            Ok(pq) => pq,
            Err(e) => panic!("invalid static path and query {src:?}: {e}"),
        }
    }

    /// Returns the path.
    pub fn path(&self) -> &Path {
        Path::from_str_unchecked(split_at_char(&self.0, '?').0)
    }

    /// Returns the query, if present.
    pub fn query(&self) -> Option<&Query> {
        split_at_char(&self.0, '?').1.map(Query::from_str_unchecked)
    }
}
