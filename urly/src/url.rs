use std::any::Any;
use std::borrow::{Borrow, Cow};
use std::ops::{Add, AddAssign, Div};
use std::str::FromStr;

use ntex_bytes::{ByteString, Bytes, BytesMut};
use simdutf8::compat::{Utf8Error, from_utf8};

use crate::authority::{self, Authority, Port, UserInfo};
use crate::error::{ErrorKind, InvalidUrl};
use crate::path::{Path, PathAndQuery};
use crate::query::{self, Fragment, Query, split_pair};
use crate::quoting::{Component, quote, requote, unquote};
use crate::{chars::lowercase, host::normalize_host, parse, scheme::Scheme};

/// Normalized URL reference.
///
/// The URL is stored in a single shared [`ByteString`]; accessors return
/// borrowed, percent-encoded components. Parsing is lenient and normalizes the
/// input the same way Python's `yarl` does:
///
/// * leading and trailing whitespace and C0 controls, tabs and newlines are removed
/// * scheme and host are lowercased, non-ASCII hosts are punycode-encoded
/// * percent-encoding is normalized: invalid characters are encoded, escapes of
///   unreserved characters are decoded, hex digits are uppercased
/// * dot segments are removed from absolute paths
/// * an empty path of `http`, `https`, `ws` and `wss` URLs becomes `/`
///
/// The normalized URL always passes strict [`Url::validate`].
///
/// ```
/// use urly::Url;
///
/// let url: Url = "HTTPS://User@Example.COM:8443/a/./b/../c d?q=1#frag".parse().unwrap();
/// assert_eq!(url, "https://User@example.com:8443/a/c%20d?q=1#frag");
/// assert_eq!(url.scheme_str(), Some("https"));
/// assert_eq!(url.host(), Some("example.com"));
/// assert_eq!(url.port_u16(), Some(8443));
/// assert_eq!(url.path(), "/a/c%20d");
/// assert_eq!(url.path().decode(), "/a/c d");
/// assert_eq!(url.query().unwrap().get("q").unwrap(), "1");
/// assert_eq!(url.fragment().unwrap(), "frag");
/// ```
///
/// URLs are limited to 65535 bytes; operations that can't return an error panic if
/// the result exceeds the limit.
#[derive(Clone)]
pub struct Url {
    data: ByteString,
    // index of ':', 0 if there is no scheme
    scheme_end: u16,
    // index after "//", 0 if there is no authority
    auth_start: u16,
    path_start: u16,
    path_end: u16,
    query_end: u16,
    // host range within the authority, both 0 if there is no authority
    host_start: u16,
    host_end: u16,
}

/// Borrowed URL components, used to assemble a new URL.
#[derive(Copy, Clone, Default)]
pub(crate) struct Components<'a> {
    pub(crate) scheme: Option<&'a str>,
    pub(crate) authority: Option<&'a str>,
    pub(crate) path: &'a str,
    pub(crate) query: Option<&'a str>,
    pub(crate) fragment: Option<&'a str>,
}

/// Assembles normalized components into a URL.
///
/// The path is adjusted so the result can be parsed back unambiguously. `orig`
/// is reused if it is equal to the result.
pub(crate) fn assemble(c: &Components<'_>, orig: Option<&ByteString>) -> Result<Url, InvalidUrl> {
    let prefix = if c.authority.is_some() {
        if c.path.is_empty() {
            let special = c
                .scheme
                .is_some_and(|s| Scheme::from_str_unchecked(s).is_special());
            if special { "/" } else { "" }
        } else if c.path.starts_with('/') {
            ""
        } else {
            "/"
        }
    } else if c.path.starts_with("//") {
        "/."
    } else if c.scheme.is_none() && c.path.split('/').next().is_some_and(|s| s.contains(':')) {
        "./"
    } else {
        ""
    };

    let pieces = [
        c.scheme.unwrap_or(""),
        if c.scheme.is_some() { ":" } else { "" },
        if c.authority.is_some() { "//" } else { "" },
        c.authority.unwrap_or(""),
        prefix,
        c.path,
        if c.query.is_some() { "?" } else { "" },
        c.query.unwrap_or(""),
        if c.fragment.is_some() { "#" } else { "" },
        c.fragment.unwrap_or(""),
    ];
    let len: usize = pieces.iter().map(|p| p.len()).sum();
    if len > parse::MAX_LEN {
        return Err(InvalidUrl::new(ErrorKind::TooLong));
    }

    let reuse = orig.filter(|orig| {
        orig.len() == len && {
            // the lengths add up to `len`, indexing can't panic. most pieces are
            // empty, and comparing empty slices is surprisingly slow
            let orig = orig.as_str().as_bytes();
            let mut pos = 0;
            pieces.iter().all(|p| {
                let start = pos;
                pos += p.len();
                p.is_empty() || orig[start..pos] == *p.as_bytes()
            })
        }
    });
    let data = match reuse {
        Some(orig) => orig.clone(),
        None => concat(&pieces, len),
    };

    // all lengths fit, the total is at most `MAX_LEN`
    let lens = pieces.map(|p| p.len() as u16);
    let scheme_end = lens[0];
    let auth_start = if c.authority.is_some() {
        lens[0] + lens[1] + lens[2]
    } else {
        0
    };
    // the `/.` prefix of an authority-less `//` path is not part of the path
    let hidden = if prefix == "/." { lens[4] } else { 0 };
    let path_start = lens[..4].iter().sum::<u16>() + hidden;
    let path_end = lens[..6].iter().sum::<u16>();
    let query_end = path_end + lens[6] + lens[7];
    let (host_start, host_end) = match c.authority {
        Some(a) => {
            let host = authority::split(a).1;
            let start = auth_start + authority::offset(a, host) as u16;
            (start, start + host.len() as u16)
        }
        None => (0, 0),
    };
    Ok(Url {
        data,
        scheme_end,
        auth_start,
        path_start,
        path_end,
        query_end,
        host_start,
        host_end,
    })
}

/// Concatenates `pieces` of total length `len` with a single copy.
fn concat(pieces: &[&str], len: usize) -> ByteString {
    // short urls are stored inline in `Bytes`, without allocation
    const INLINE: usize = 23;

    let bytes = if len <= INLINE {
        let mut buf = [0u8; INLINE];
        let mut pos = 0;
        for p in pieces {
            buf[pos..pos + p.len()].copy_from_slice(p.as_bytes());
            pos += p.len();
        }
        Bytes::copy_from_slice(&buf[..len])
    } else {
        let mut buf = BytesMut::with_capacity(len);
        for p in pieces {
            buf.extend_from_slice(p.as_bytes());
        }
        buf.freeze()
    };
    // SAFETY: a concatenation of strings is valid UTF-8
    unsafe { ByteString::from_bytes_unchecked(bytes) }
}

fn too_long<T>(res: Result<T, InvalidUrl>) -> T {
    match res {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

impl Url {
    /// Returns the relative URL `/`.
    pub const fn new() -> Url {
        Url {
            data: ByteString::from_static("/"),
            scheme_end: 0,
            auth_start: 0,
            path_start: 0,
            path_end: 1,
            query_end: 1,
            host_start: 0,
            host_end: 0,
        }
    }

    /// Parses and normalizes an authority-form `[userinfo@]host[:port]`, like
    /// `http::Uri` does for a string without a scheme or a leading `/`.
    ///
    /// The result is a network-path reference without a path. Use
    /// [`Url::parse_ref`] to parse any URL reference, it treats such a string
    /// as a relative path.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let url = Url::parse("Custom.Domain:8080").unwrap();
    /// assert_eq!(url, "//custom.domain:8080");
    /// assert_eq!(url.host(), Some("custom.domain"));
    /// assert_eq!(url.port_u16(), Some(8080));
    /// assert_eq!(url.path(), "");
    ///
    /// assert!(Url::parse("custom.domain/path").is_err());
    /// assert_eq!(Url::parse_ref("custom.domain").unwrap().host(), None);
    /// ```
    pub fn parse<T: AsRef<str>>(src: T) -> Result<Url, InvalidUrl> {
        parse::parse_authority(src.as_ref())
    }

    /// Parses and normalizes a URL reference.
    ///
    /// Same as `Url::try_from()` and `str::parse()`.
    pub fn parse_ref<T: AsRef<str>>(src: T) -> Result<Url, InvalidUrl> {
        parse::parse(src.as_ref(), None)
    }

    /// Strictly validates a URI reference according to RFC 3986.
    ///
    /// Unlike parsing, validation does not normalize: whitespace, non-ASCII
    /// characters and invalid escapes are errors. The error position is a byte
    /// offset into `src`. Input that is not valid UTF-8 is rejected with
    /// [`ErrorKind::InvalidChar`](crate::ErrorKind::InvalidChar) at its first
    /// non-ASCII byte.
    ///
    /// ```
    /// use urly::{ErrorKind, Url};
    ///
    /// assert!(Url::validate("https://example.com/a%20b?q#f").is_ok());
    /// assert!(Url::validate(b"/path?q").is_ok());
    ///
    /// let err = Url::validate("http://ex ample.com").unwrap_err();
    /// assert_eq!(err.kind(), ErrorKind::InvalidChar(' '));
    /// assert_eq!(err.position(), Some(9));
    ///
    /// let err = Url::validate(b"/a\xff").unwrap_err();
    /// assert_eq!(err.kind(), ErrorKind::InvalidChar(char::REPLACEMENT_CHARACTER));
    /// assert_eq!(err.position(), Some(2));
    /// ```
    pub fn validate<T: AsRef<[u8]>>(src: T) -> Result<(), InvalidUrl> {
        let src = src.as_ref();
        if let Ok(s) = from_utf8(src) {
            return parse::validate(s);
        }
        let i = src.iter().position(|b| !b.is_ascii()).unwrap_or_default();
        let c = src[i..]
            .utf8_chunks()
            .next()
            .and_then(|chunk| chunk.valid().chars().next())
            .unwrap_or(char::REPLACEMENT_CHARACTER);
        Err(InvalidUrl::at(ErrorKind::InvalidChar(c), i))
    }

    /// Converts a static string to a URL. The string is not copied if it is
    /// already normalized.
    ///
    /// # Panics
    ///
    /// Panics if the URL is not valid.
    pub fn from_static(src: &'static str) -> Url {
        match parse::parse(src, Some(&ByteString::from_static(src))) {
            Ok(url) => url,
            Err(e) => panic!("invalid static url {src:?}: {e}"),
        }
    }

    /// Converts a `Bytes`, `String`, `Vec<u8>` or any other
    /// byte buffer to a URL, reusing the buffer if possible.
    pub fn from_maybe_shared<T>(src: T) -> Result<Url, InvalidUrl>
    where
        T: AsRef<[u8]> + 'static,
    {
        let mut src = Some(src);
        let any = &mut src as &mut dyn Any;
        if let Some(src) = any.downcast_mut::<Option<Bytes>>() {
            return Url::try_from(src.take().unwrap());
        }
        if let Some(src) = any.downcast_mut::<Option<String>>() {
            return Url::try_from(src.take().unwrap());
        }
        if let Some(src) = any.downcast_mut::<Option<Vec<u8>>>() {
            return Url::try_from(Bytes::from(src.take().unwrap()));
        }
        Url::try_from(src.unwrap().as_ref())
    }

    /// Returns a new [`Builder`](crate::Builder).
    pub fn builder() -> crate::Builder {
        crate::Builder::new()
    }

    pub(crate) fn empty() -> Url {
        Url {
            data: ByteString::new(),
            scheme_end: 0,
            auth_start: 0,
            path_start: 0,
            path_end: 0,
            query_end: 0,
            host_start: 0,
            host_end: 0,
        }
    }

    fn components(&self) -> Components<'_> {
        Components {
            scheme: self.scheme_str(),
            authority: self.authority().map(Authority::as_str),
            path: self.path().as_str(),
            query: self.query().map(Query::as_str),
            fragment: self.fragment().map(Fragment::as_str),
        }
    }

    fn range(&self, start: u16, end: u16) -> &str {
        &self.data[start as usize..end as usize]
    }

    // ===== accessors =====

    /// Returns the URL as a string.
    pub fn as_str(&self) -> &str {
        &self.data
    }

    /// Returns the URL as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.data.as_bytes()
    }

    /// Returns the underlying buffer.
    pub fn as_byte_string(&self) -> &ByteString {
        &self.data
    }

    /// Returns the scheme, if present.
    pub fn scheme(&self) -> Option<&Scheme> {
        self.scheme_str().map(Scheme::from_str_unchecked)
    }

    /// Returns the scheme as a string, if present.
    pub fn scheme_str(&self) -> Option<&str> {
        (self.scheme_end > 0).then(|| self.range(0, self.scheme_end))
    }

    /// Returns the authority, if present.
    pub fn authority(&self) -> Option<&Authority> {
        (self.auth_start > 0)
            .then(|| Authority::from_str_unchecked(self.range(self.auth_start, self.path_start)))
    }

    /// Returns the userinfo, if present.
    pub fn userinfo(&self) -> Option<&UserInfo> {
        (self.host_start > self.auth_start)
            .then(|| UserInfo::from_str_unchecked(self.range(self.auth_start, self.host_start - 1)))
    }

    /// Returns the decoded user name, if present.
    pub fn username(&self) -> Option<Cow<'_, str>> {
        self.userinfo().map(UserInfo::decoded_username)
    }

    /// Returns the decoded password, if present.
    pub fn password(&self) -> Option<Cow<'_, str>> {
        self.userinfo()?.decoded_password()
    }

    /// Returns the host, if present and not empty. IPv6 addresses include the
    /// brackets, non-ASCII domains are punycode-encoded.
    pub fn host(&self) -> Option<&str> {
        (self.host_end > self.host_start).then(|| self.range(self.host_start, self.host_end))
    }

    /// Returns the parsed host, if present and not empty.
    ///
    /// ```
    /// use std::net::Ipv4Addr;
    /// use urly::{Host, Url};
    ///
    /// let url = Url::from_static("http://127.0.0.1/");
    /// assert_eq!(url.host_parsed(), Some(Host::Ipv4(Ipv4Addr::LOCALHOST)));
    ///
    /// let url = Url::from_static("http://münchen.de/");
    /// assert_eq!(url.host(), Some("xn--mnchen-3ya.de"));
    /// assert_eq!(url.host_parsed().unwrap().to_unicode(), "münchen.de");
    /// ```
    pub fn host_parsed(&self) -> Option<crate::Host<'_>> {
        self.host().map(crate::Host::classify)
    }

    /// Returns the explicit port, if present.
    pub fn port(&self) -> Option<Port<&str>> {
        if self.auth_start > 0 && self.host_end < self.path_start {
            Port::parse(self.range(self.host_end + 1, self.path_start))
        } else {
            None
        }
    }

    /// Returns the explicit port as a number, if present.
    pub fn port_u16(&self) -> Option<u16> {
        self.port().map(|p| p.as_u16())
    }

    /// Returns the explicit port, or the default port of the scheme.
    pub fn port_or_known_default(&self) -> Option<u16> {
        self.port_u16().or_else(|| self.scheme()?.default_port())
    }

    /// Returns the percent-encoded path.
    ///
    /// A path starting with `//` in a URL without authority is serialized with a
    /// `/.` prefix, which is not part of the path.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let url = Url::from_static("/.//a?q");
    /// assert_eq!(url.path(), "//a");
    /// assert_eq!(url.path_and_query(), "//a?q");
    /// ```
    pub fn path(&self) -> &Path {
        Path::from_str_unchecked(self.range(self.path_start, self.path_end))
    }

    /// Returns the percent-encoded query, if present.
    pub fn query(&self) -> Option<&Query> {
        (self.query_end > self.path_end)
            .then(|| Query::from_str_unchecked(self.range(self.path_end + 1, self.query_end)))
    }

    /// Returns the path and query.
    pub fn path_and_query(&self) -> &PathAndQuery {
        PathAndQuery::from_str_unchecked(self.range(self.path_start, self.query_end))
    }

    /// Returns the percent-encoded fragment, if present.
    pub fn fragment(&self) -> Option<&Fragment> {
        ((self.query_end as usize) < self.data.len())
            .then(|| Fragment::from_str_unchecked(&self.data[self.query_end as usize + 1..]))
    }

    /// Returns `true` if the URL has a scheme.
    pub fn is_absolute(&self) -> bool {
        self.scheme_end > 0
    }

    /// Returns `true` if the port is absent or is the default port of the scheme.
    pub fn is_default_port(&self) -> bool {
        match self.port_u16() {
            None => true,
            Some(port) => self.scheme().and_then(Scheme::default_port) == Some(port),
        }
    }

    // ===== derived urls =====

    /// Returns the origin: scheme, host and port.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let url = Url::from_static("https://user:pw@example.com:8443/a?b#c");
    /// assert_eq!(url.origin().unwrap(), "https://example.com:8443/");
    /// assert!(Url::from_static("/a").origin().is_none());
    /// ```
    pub fn origin(&self) -> Option<Url> {
        let scheme = self.scheme_str()?;
        let authority = self.authority()?;
        if authority.host().is_empty() {
            return None;
        }
        Some(self.derive(|c| {
            *c = Components {
                scheme: Some(scheme),
                authority: Some(authority.host_port()),
                ..Components::default()
            };
        }))
    }

    /// Returns the relative part of the URL: path, query and fragment.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let url = Url::from_static("https://example.com/a?b#c");
    /// assert_eq!(url.relative(), "/a?b#c");
    /// ```
    pub fn relative(&self) -> Url {
        self.derive(|c| {
            c.scheme = None;
            c.authority = None;
        })
    }

    /// Returns the URL with the last path segment removed, without query and
    /// fragment.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// assert_eq!(Url::from_static("http://h/a/b?q").parent(), "http://h/a");
    /// assert_eq!(Url::from_static("http://h/a/b/").parent(), "http://h/a");
    /// assert_eq!(Url::from_static("http://h/a").parent(), "http://h/");
    /// assert_eq!(Url::from_static("http://h/").parent(), "http://h/");
    /// ```
    pub fn parent(&self) -> Url {
        let path = self.path().as_str();
        let trimmed = path.trim_end_matches('/');
        let parent = match trimmed.rfind('/') {
            Some(0) => "/",
            Some(i) => &trimmed[..i],
            None if path.starts_with('/') => "/",
            None => "",
        };
        self.with_path(parent)
    }

    /// Appends literal path segments, without query and fragment.
    ///
    /// Segments are percent-encoded; `/` is kept and a leading `/` is ignored.
    /// The same operation is available as the `/` operator.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let url = Url::from_static("http://h/api?q");
    /// assert_eq!(url.push_segments(["v1", "a b"]), "http://h/api/v1/a%20b");
    /// assert_eq!(&url / "users/" / "1", "http://h/api/users/1");
    /// ```
    pub fn push_segments<I, S>(&self, segments: I) -> Url
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut path = self.path().as_str().to_string();
        for segment in segments {
            let segment = segment.as_ref();
            let segment = segment.strip_prefix('/').unwrap_or(segment);
            if !path.is_empty() && !path.ends_with('/') {
                path.push('/');
            }
            path.push_str(&quote(segment, Component::Path));
        }
        if path.starts_with('/') && parse::has_dot_segments(&path) {
            path = parse::remove_dot_segments(&path);
        }
        self.with_path(&path)
    }

    /// Resolves a reference against this URL, RFC 3986 section 5.2.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let base = Url::from_static("http://a/b/c/d;p?q");
    /// assert_eq!(base.join("../g").unwrap(), "http://a/b/g");
    /// assert_eq!(base.join("//g/x").unwrap(), "http://g/x");
    /// assert_eq!(base.join("?y").unwrap(), "http://a/b/c/d;p?y");
    /// ```
    pub fn join(&self, reference: &str) -> Result<Url, InvalidUrl> {
        if reference.trim_matches(|c: char| c <= ' ').is_empty() {
            return self.rebuild(|c| c.fragment = None);
        }
        Ok(self.join_url(&Url::parse_ref(reference)?))
    }

    /// Resolves a parsed reference against this URL, RFC 3986 section 5.2.2.
    ///
    /// The `+` and `+=` operators are equivalent.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let base = Url::from_static("http://a/b/c/d;p?q");
    /// let r = Url::from_static("../g");
    /// assert_eq!(base.join_url(&r), "http://a/b/g");
    /// assert_eq!(&base + &r, "http://a/b/g");
    ///
    /// let mut url = base.clone();
    /// url += r;
    /// assert_eq!(url, "http://a/b/g");
    /// ```
    pub fn join_url(&self, r: &Url) -> Url {
        let base = self.components();
        let r = r.components();
        let resolved = |path| Cow::Owned(parse::remove_dot_segments(path));
        let (scheme, authority, path, query) = if r.scheme.is_some() {
            (r.scheme, r.authority, resolved(r.path), r.query)
        } else if r.authority.is_some() {
            (base.scheme, r.authority, resolved(r.path), r.query)
        } else if r.path.is_empty() {
            let query = r.query.or(base.query);
            (base.scheme, base.authority, Cow::Borrowed(base.path), query)
        } else if r.path.starts_with('/') {
            (base.scheme, base.authority, resolved(r.path), r.query)
        } else {
            let dir = if base.authority.is_some() && base.path.is_empty() {
                "/"
            } else {
                dir(base.path)
            };
            let path = resolved(&format!("{dir}{}", r.path));
            (base.scheme, base.authority, path, r.query)
        };
        self.derive(|c| {
            *c = Components {
                scheme,
                authority,
                path: &path,
                query,
                fragment: r.fragment,
            };
        })
    }

    // ===== setters =====

    /// Assembles a URL from modified components, reusing the buffer if
    /// nothing changed.
    fn rebuild<'a>(&'a self, f: impl FnOnce(&mut Components<'a>)) -> Result<Url, InvalidUrl> {
        let mut c = self.components();
        f(&mut c);
        assemble(&c, Some(&self.data))
    }

    fn derive<'a>(&'a self, f: impl FnOnce(&mut Components<'a>)) -> Url {
        too_long(self.rebuild(f))
    }

    /// Returns the URL with a new path, without query and fragment.
    fn with_path(&self, path: &str) -> Url {
        self.derive(|c| {
            c.path = path;
            c.query = None;
            c.fragment = None;
        })
    }

    /// Sets the scheme.
    pub fn set_scheme(&mut self, scheme: &str) -> Result<(), InvalidUrl> {
        let scheme = lowercase(Scheme::new(scheme)?.as_str().into());
        self.rebuild(|c| c.scheme = Some(&scheme))
            .map(|url| *self = url)
    }

    /// Sets or removes the raw authority: `[userinfo "@"] host [":" port]`.
    pub fn set_authority(&mut self, authority: Option<&str>) -> Result<(), InvalidUrl> {
        let authority = authority.map(parse::normalize_authority).transpose()?;
        self.rebuild(|c| c.authority = authority.as_deref())
            .map(|url| *self = url)
    }

    fn with_authority(
        &self,
        userinfo: Option<&str>,
        host: &str,
        port: Option<u16>,
    ) -> Result<Url, InvalidUrl> {
        let authority = parse::join_authority(userinfo, host, port);
        self.rebuild(|c| c.authority = Some(&authority))
    }

    fn host_parts(&self) -> Result<(Option<&str>, &str, Option<u16>), InvalidUrl> {
        let authority = self
            .authority()
            .ok_or(InvalidUrl::new(ErrorKind::AuthorityMissing))?;
        let (userinfo, host, _) = authority::split(authority.as_str());
        if host.is_empty() {
            return Err(InvalidUrl::new(ErrorKind::InvalidHost));
        }
        Ok((userinfo, host, authority.port_u16()))
    }

    /// Sets or removes the literal user name and password.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://example.com/");
    /// url.set_userinfo(Some("us@r"), Some("p:w")).unwrap();
    /// assert_eq!(url, "http://us%40r:p%3Aw@example.com/");
    /// assert_eq!(url.password().unwrap(), "p:w");
    /// ```
    pub fn set_userinfo(
        &mut self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<(), InvalidUrl> {
        let (_, host, port) = self.host_parts()?;
        let userinfo = match (username, password) {
            (None, None) => None,
            (user, None) => Some(quote(user.unwrap_or(""), Component::UserInfo)),
            (user, Some(password)) => Some(Cow::Owned(format!(
                "{}:{}",
                quote(user.unwrap_or(""), Component::UserInfo),
                quote(password, Component::UserInfo)
            ))),
        };
        self.with_authority(userinfo.as_deref(), host, port)
            .map(|url| *self = url)
    }

    /// Sets the host. Escapes are decoded, non-ASCII domains are
    /// punycode-encoded, IPv6 addresses may be given without brackets.
    ///
    /// An authority is added if the URL doesn't have one.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://user@example.com:8080/a");
    /// url.set_host("::1").unwrap();
    /// assert_eq!(url, "http://user@[::1]:8080/a");
    /// url.set_host("Bücher.example").unwrap();
    /// assert_eq!(url, "http://user@xn--bcher-kva.example:8080/a");
    /// ```
    pub fn set_host(&mut self, host: &str) -> Result<(), InvalidUrl> {
        let bracketed;
        let host = if host.contains(':') && !host.starts_with('[') {
            bracketed = format!("[{host}]");
            &bracketed
        } else {
            host
        };
        let host = normalize_host(host)?;
        if host.is_empty() {
            return Err(InvalidUrl::new(ErrorKind::InvalidHost));
        }
        let (userinfo, port) = match self.authority() {
            Some(a) => (a.userinfo().map(UserInfo::as_str), a.port_u16()),
            None => (None, None),
        };
        self.with_authority(userinfo, &host, port)
            .map(|url| *self = url)
    }

    /// Sets or removes the port.
    pub fn set_port(&mut self, port: Option<u16>) -> Result<(), InvalidUrl> {
        let (userinfo, host, _) = self.host_parts()?;
        self.with_authority(userinfo, host, port)
            .map(|url| *self = url)
    }

    /// Sets the percent-encoded path, keeping query and fragment.
    ///
    /// Invalid characters are encoded and dot segments are removed.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://h/a?q");
    /// url.set_path("/b/../c d");
    /// assert_eq!(url, "http://h/c%20d?q");
    /// ```
    pub fn set_path(&mut self, path: &str) {
        let prefixed;
        let path = if self.auth_start != 0 && !path.is_empty() && !path.starts_with('/') {
            prefixed = format!("/{path}");
            &prefixed
        } else {
            path
        };
        let path = parse::normalize_path(path);
        *self = self.derive(|c| c.path = &path);
    }

    /// Replaces the last path segment with a literal file name.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://h/a/b.txt?q");
    /// url.set_file_name("c d.html").unwrap();
    /// assert_eq!(url, "http://h/a/c%20d.html?q");
    /// ```
    pub fn set_file_name(&mut self, name: &str) -> Result<(), InvalidUrl> {
        if name.contains('/') || name == "." || name == ".." {
            return Err(InvalidUrl::new(ErrorKind::InvalidPath));
        }
        self.with_file_name(&quote(name, Component::Path))
            .map(|url| *self = url)
    }

    /// Replaces the last path segment with an encoded file name.
    fn with_file_name(&self, name: &str) -> Result<Url, InvalidUrl> {
        let path = format!("{}{name}", dir(self.path().as_str()));
        self.rebuild(|c| c.path = &path)
    }

    /// Replaces the extension of the file name; an empty extension removes it.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://h/a/b.txt");
    /// url.set_extension("tar.gz").unwrap();
    /// assert_eq!(url, "http://h/a/b.tar.gz");
    /// url.set_extension("").unwrap();
    /// assert_eq!(url, "http://h/a/b.tar");
    /// ```
    pub fn set_extension(&mut self, extension: &str) -> Result<(), InvalidUrl> {
        let stem = self
            .path()
            .file_stem()
            .ok_or(InvalidUrl::new(ErrorKind::InvalidPath))?;
        if extension.contains('/') {
            return Err(InvalidUrl::new(ErrorKind::InvalidPath));
        }
        let name = if extension.is_empty() {
            Cow::Borrowed(stem)
        } else {
            let extension = quote(extension, Component::Path);
            Cow::Owned(format!("{stem}.{extension}"))
        };
        self.with_file_name(&name).map(|url| *self = url)
    }

    /// Sets or removes the percent-encoded query.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://h/#f");
    /// url.set_query(Some("a=b c&d"));
    /// assert_eq!(url, "http://h/?a=b+c&d#f");
    /// ```
    pub fn set_query(&mut self, query: Option<&str>) {
        let query = query.map(|q| requote(q, Component::Query));
        *self = self.derive(|c| c.query = query.as_deref());
    }

    /// Keeps existing pairs whose decoded key passes `keep` and appends literal
    /// `pairs`. An empty result removes the query.
    fn edit_query_pairs<I, K, V>(&mut self, keep: impl Fn(&str) -> bool, pairs: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let existing = self.query().map_or("", Query::as_str);
        let pieces: Vec<Cow<'_, str>> = query::pieces(existing)
            .filter(|piece| keep(&unquote(split_pair(piece).0, Component::QueryPart)))
            .map(Cow::Borrowed)
            .chain(pairs.into_iter().map(|(k, v)| {
                let (k, v) = (k.as_ref(), v.as_ref());
                let (k, v) = (
                    quote(k, Component::QueryPart),
                    quote(v, Component::QueryPart),
                );
                Cow::Owned(format!("{k}={v}"))
            }))
            .collect();
        let query = pieces.join("&");
        *self = self.derive(|c| c.query = (!query.is_empty()).then_some(query.as_str()));
    }

    /// Replaces the query with literal key-value pairs. Empty pairs remove the query.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://h/?old");
    /// url.set_query_pairs([("a", "1 2"), ("b&", "=")]);
    /// assert_eq!(url, "http://h/?a=1+2&b%26=%3D");
    /// ```
    pub fn set_query_pairs<I, K, V>(&mut self, pairs: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.edit_query_pairs(|_| false, pairs);
    }

    /// Appends literal key-value pairs to the query.
    pub fn extend_query_pairs<I, K, V>(&mut self, pairs: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.edit_query_pairs(|_| true, pairs);
    }

    /// Replaces all values of the given keys, appending the new pairs.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://h/?a=1&b=2&a=3");
    /// url.update_query_pairs([("a", "4")]);
    /// assert_eq!(url, "http://h/?b=2&a=4");
    /// ```
    pub fn update_query_pairs<I, K, V>(&mut self, pairs: I)
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let pairs: Vec<(K, V)> = pairs.into_iter().collect();
        let keep = |key: &str| !pairs.iter().any(|(k, _)| k.as_ref() == key);
        self.edit_query_pairs(keep, pairs.iter().map(|(k, v)| (k.as_ref(), v.as_ref())));
    }

    /// Removes all pairs with the given keys from the query.
    ///
    /// ```
    /// use urly::Url;
    ///
    /// let mut url = Url::from_static("http://h/?a=1&b=2&a=3");
    /// url.remove_query_params(["a"]);
    /// assert_eq!(url, "http://h/?b=2");
    /// url.remove_query_params(["b"]);
    /// assert_eq!(url, "http://h/");
    /// ```
    pub fn remove_query_params<I, K>(&mut self, keys: I)
    where
        I: IntoIterator<Item = K>,
        K: AsRef<str>,
    {
        let keys: Vec<K> = keys.into_iter().collect();
        let keep = |key: &str| !keys.iter().any(|k| k.as_ref() == key);
        self.edit_query_pairs(keep, std::iter::empty::<(&str, &str)>());
    }

    /// Sets or removes the percent-encoded fragment.
    pub fn set_fragment(&mut self, fragment: Option<&str>) {
        let fragment = fragment.map(|f| requote(f, Component::Fragment));
        *self = self.derive(|c| c.fragment = fragment.as_deref());
    }

    /// Splits the URL into its parts.
    pub fn into_parts(self) -> crate::Parts {
        let slice = |start: u16, end: usize| self.data.slice(start as usize..end);
        crate::Parts {
            scheme: self.scheme().map(|_| slice(0, self.scheme_end as usize)),
            authority: self
                .authority()
                .map(|_| slice(self.auth_start, self.path_start as usize)),
            path_and_query: slice(self.path_start, self.query_end as usize),
            fragment: self
                .fragment()
                .map(|_| slice(self.query_end + 1, self.data.len())),
        }
    }

    /// Converts the URL into its buffer.
    pub fn into_byte_string(self) -> ByteString {
        self.data
    }
}

/// Returns the path up to and including the last `/`.
fn dir(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..=i])
}

// ===== trait impls =====

str_fmt!(Url);
str_eq!(Url);

/// Returns the relative URL `/`, like `http::Uri::default()`.
impl Default for Url {
    fn default() -> Url {
        Url::new()
    }
}

impl FromStr for Url {
    type Err = InvalidUrl;

    fn from_str(s: &str) -> Result<Url, InvalidUrl> {
        parse::parse(s, None)
    }
}

macro_rules! try_from {
    ($($ty:ty => |$s:ident| $conv:expr;)*) => {$(
        impl TryFrom<$ty> for Url {
            type Error = InvalidUrl;

            fn try_from($s: $ty) -> Result<Url, InvalidUrl> {
                $conv
            }
        }
    )*};
}

try_from! {
    &str => |s| parse::parse(s, None);
    &String => |s| parse::parse(s, None);
    // `ByteString::from(String)` copies, assembling copies only once
    String => |s| parse::parse(&s, None);
    ByteString => |s| parse::parse(&s, Some(&s));
    &ByteString => |s| parse::parse(s, Some(s));
    &[u8] => |s| parse::parse(from_utf8(s).map_err(utf8_error)?, None);
    Bytes => |s| {
        from_utf8(&s).map_err(utf8_error)?;
        // SAFETY: validated above
        Url::try_from(unsafe { ByteString::from_bytes_unchecked(s) })
    };
}

#[allow(clippy::needless_pass_by_value)]
fn utf8_error(e: Utf8Error) -> InvalidUrl {
    InvalidUrl::at(ErrorKind::InvalidChar('\u{FFFD}'), e.valid_up_to())
}

impl From<Url> for ByteString {
    fn from(url: Url) -> ByteString {
        url.data
    }
}

impl From<Url> for String {
    fn from(url: Url) -> String {
        url.as_str().to_string()
    }
}

impl<S: AsRef<str>> Div<S> for &Url {
    type Output = Url;

    fn div(self, segment: S) -> Url {
        self.push_segments([segment])
    }
}

impl<S: AsRef<str>> Div<S> for Url {
    type Output = Url;

    fn div(self, segment: S) -> Url {
        self.push_segments([segment])
    }
}

impl<U: Borrow<Url>> Add<U> for &Url {
    type Output = Url;

    fn add(self, reference: U) -> Url {
        self.join_url(reference.borrow())
    }
}

impl<U: Borrow<Url>> Add<U> for Url {
    type Output = Url;

    fn add(self, reference: U) -> Url {
        self.join_url(reference.borrow())
    }
}

impl<U: Borrow<Url>> AddAssign<U> for Url {
    fn add_assign(&mut self, reference: U) {
        *self = self.join_url(reference.borrow());
    }
}

#[cfg(feature = "http")]
mod http_impls {
    use http::uri::{InvalidUri, Uri};

    use super::Url;
    use crate::error::InvalidUrl;

    impl TryFrom<&Uri> for Url {
        type Error = InvalidUrl;

        fn try_from(uri: &Uri) -> Result<Url, InvalidUrl> {
            if uri.scheme().is_none()
                && let Some(authority) = uri.authority()
            {
                return Url::parse(authority.as_str());
            }
            let uri = uri.to_string();
            // an origin-form path starting with `//` is not an authority
            if uri.starts_with("//") {
                Url::try_from(format!("/.{uri}"))
            } else {
                Url::try_from(uri)
            }
        }
    }

    impl TryFrom<Uri> for Url {
        type Error = InvalidUrl;

        fn try_from(uri: Uri) -> Result<Url, InvalidUrl> {
            Url::try_from(&uri)
        }
    }

    /// The fragment is removed, `Uri` doesn't support it. A network-path
    /// reference converts to authority-form, it fails if it has a path or
    /// query.
    impl TryFrom<&Url> for Uri {
        type Error = InvalidUri;

        fn try_from(url: &Url) -> Result<Uri, InvalidUri> {
            // `Uri` parses a relative reference as origin-form, without the `/.`
            // prefix, and a reference without scheme and `//` as authority-form
            let start = match (url.scheme_end, url.auth_start) {
                (0, 0) => url.path_start,
                (0, auth_start) => auth_start,
                _ => 0,
            };
            Uri::try_from(url.range(start, url.query_end))
        }
    }

    /// The fragment is removed, `Uri` doesn't support it.
    impl TryFrom<Url> for Uri {
        type Error = InvalidUri;

        fn try_from(url: Url) -> Result<Uri, InvalidUri> {
            Uri::try_from(&url)
        }
    }
}

#[cfg(feature = "serde")]
mod serde_impls {
    use std::fmt;

    use serde::de::{self, Deserialize, Deserializer, Visitor};
    use serde::{Serialize, Serializer};

    use super::Url;

    impl Serialize for Url {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_str(self.as_str())
        }
    }

    struct UrlVisitor;

    impl Visitor<'_> for UrlVisitor {
        type Value = Url;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a url")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Url, E> {
            Url::try_from(v).map_err(E::custom)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Url, E> {
            Url::try_from(v).map_err(E::custom)
        }
    }

    impl<'de> Deserialize<'de> for Url {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Url, D::Error> {
            deserializer.deserialize_str(UrlVisitor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default() {
        const URL: Url = Url::new();
        assert_eq!(URL, Url::default());
        let (url, p) = (Url::default(), Url::from_static("/"));
        assert_eq!(url.scheme_end, p.scheme_end);
        assert_eq!(url.auth_start, p.auth_start);
        assert_eq!(url.path_start, p.path_start);
        assert_eq!(url.path_end, p.path_end);
        assert_eq!(url.query_end, p.query_end);
        assert_eq!((url.host_start, url.host_end), (p.host_start, p.host_end));
        assert_eq!(url, "/");
        assert_eq!(url.path(), "/");
        assert!(url.query().is_none());
        assert!(!url.is_absolute());
    }

    #[test]
    fn cached_host() {
        let cases = [
            (
                "http://u:p@[::1]:8080/a",
                Some("u:p"),
                Some("[::1]"),
                Some(8080),
            ),
            ("http://@h/", Some(""), Some("h"), None),
            ("http://h:0/", None, Some("h"), Some(0)),
            ("file:///etc", None, None, None),
            ("//h", None, Some("h"), None),
            ("/a", None, None, None),
        ];
        for (src, userinfo, host, port) in cases {
            let url = Url::from_static(src);
            let auth = url.authority();
            assert_eq!(url.userinfo().map(UserInfo::as_str), userinfo, "{src}");
            assert_eq!(url.host(), host, "{src}");
            assert_eq!(url.port_u16(), port, "{src}");
            assert_eq!(
                auth.and_then(|a| a.userinfo()).map(UserInfo::as_str),
                userinfo
            );
            assert_eq!(auth.map(Authority::host).filter(|h| !h.is_empty()), host);
            assert_eq!(auth.and_then(Authority::port_u16), port);
            let mut copy = url.clone();
            copy.set_path("/x");
            assert_eq!((copy.host(), copy.port_u16()), (host, port), "{src}");
        }
    }

    #[test]
    fn double_slash_path() {
        let url = Url::from_static("/.//a/../b?q#f");
        assert_eq!(url, "/.//b?q#f");
        assert_eq!(url.path(), "//b");
        assert_eq!(url.path_and_query(), "//b?q");
        assert!(url.authority().is_none());
        assert_eq!(Url::from_parts(url.clone().into_parts()).unwrap(), url);
        assert_eq!(url.join("c").unwrap().path(), "//c");

        let mut url = Url::default();
        url.set_path("//p");
        assert_eq!(url, "/.//p");
        assert_eq!(url.path(), "//p");

        let mut url = Url::from_static("http://h//p?q");
        assert_eq!(url.path(), "//p");
        url.set_authority(None).unwrap();
        assert_eq!(url, "http:/.//p?q");
        assert_eq!(url.path(), "//p");
        url.set_authority(Some("h")).unwrap();
        assert_eq!(url, "http://h//p?q");

        let parts = crate::Parts {
            path_and_query: "//p?q".into(),
            ..crate::Parts::default()
        };
        assert_eq!(Url::from_parts(parts).unwrap().path(), "//p");
    }
}
