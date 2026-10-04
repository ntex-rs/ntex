use ntex_bytes::ByteString;

use crate::authority::Authority;
use crate::error::{ErrorKind, InvalidUrl, InvalidUrlParts};
use crate::path::PathAndQuery;
use crate::query::Fragment;
use crate::scheme::Scheme;
use crate::{parse, url::Url};

/// URL builder.
///
/// Components are given literally (host, user info, query pairs) or
/// percent-encoded (path, query, fragment) and are normalized by
/// [`Builder::build`]. A builder created from a [`Url`] modifies that URL.
///
/// ```
/// use urly::Url;
///
/// let url = Url::builder()
///     .scheme("https")
///     .userinfo("user", Some("p@ss"))
///     .host("example.com")
///     .port(8443)
///     .path("/search")
///     .query_pair("q", "a b")
///     .fragment("top")
///     .build()
///     .unwrap();
/// assert_eq!(url, "https://user:p%40ss@example.com:8443/search?q=a+b#top");
///
/// let url = urly::Builder::from(url).port(443).fragment("").build().unwrap();
/// assert_eq!(url, "https://user:p%40ss@example.com:443/search?q=a+b#");
/// ```
#[derive(Debug, Default, Clone)]
pub struct Builder {
    base: Option<Url>,
    scheme: Option<String>,
    authority: Option<String>,
    host: Option<String>,
    userinfo: Option<(String, Option<String>)>,
    port: Option<u16>,
    path: Option<String>,
    query: Option<String>,
    pairs: Vec<(String, String)>,
    fragment: Option<String>,
}

impl Builder {
    /// Creates an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the scheme.
    #[must_use]
    pub fn scheme(mut self, scheme: &str) -> Self {
        self.scheme = Some(scheme.to_string());
        self
    }

    /// Sets the raw authority: `[userinfo "@"] host [":" port]`.
    ///
    /// Host, user info and port set separately are applied afterwards.
    #[must_use]
    pub fn authority(mut self, authority: &str) -> Self {
        self.authority = Some(authority.to_string());
        self
    }

    /// Sets the literal host, see [`Url::set_host`].
    #[must_use]
    pub fn host(mut self, host: &str) -> Self {
        self.host = Some(host.to_string());
        self
    }

    /// Sets the literal user name and password.
    #[must_use]
    pub fn userinfo(mut self, username: &str, password: Option<&str>) -> Self {
        self.userinfo = Some((username.to_string(), password.map(str::to_string)));
        self
    }

    /// Sets the port.
    #[must_use]
    pub fn port(mut self, port: u16) -> Self {
        self.port = Some(port);
        self
    }

    /// Sets the percent-encoded path.
    #[must_use]
    pub fn path(mut self, path: &str) -> Self {
        self.path = Some(path.to_string());
        self
    }

    /// Sets the percent-encoded query.
    #[must_use]
    pub fn query(mut self, query: &str) -> Self {
        self.query = Some(query.to_string());
        self
    }

    /// Appends a literal key-value pair to the query.
    #[must_use]
    pub fn query_pair(mut self, key: &str, value: &str) -> Self {
        self.pairs.push((key.to_string(), value.to_string()));
        self
    }

    /// Appends literal key-value pairs to the query.
    #[must_use]
    pub fn query_pairs<I, K, V>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        self.pairs.extend(
            pairs
                .into_iter()
                .map(|(k, v)| (k.as_ref().to_string(), v.as_ref().to_string())),
        );
        self
    }

    /// Sets the percent-encoded fragment.
    #[must_use]
    pub fn fragment(mut self, fragment: &str) -> Self {
        self.fragment = Some(fragment.to_string());
        self
    }

    /// Builds the URL.
    pub fn build(self) -> Result<Url, InvalidUrl> {
        let mut url = self.base.unwrap_or_else(Url::empty);
        if let Some(scheme) = self.scheme {
            url.set_scheme(&scheme)?;
        }
        if let Some(authority) = self.authority {
            url.set_authority(Some(&authority))?;
        }
        if let Some(host) = self.host {
            url.set_host(&host)?;
        }
        if let Some((user, password)) = self.userinfo {
            url.set_userinfo(Some(&user), password.as_deref())?;
        }
        if self.port.is_some() {
            url.set_port(self.port)?;
        }
        if let Some(path) = self.path {
            url.set_path(&path);
        }
        if let Some(query) = self.query {
            url.set_query(Some(&query));
        }
        if !self.pairs.is_empty() {
            url.extend_query_pairs(self.pairs);
        }
        if let Some(fragment) = self.fragment {
            url.set_fragment(Some(&fragment));
        }
        if url.as_str().is_empty() {
            Err(InvalidUrl::new(ErrorKind::Empty))
        } else {
            Ok(url)
        }
    }
}

impl From<Url> for Builder {
    fn from(url: Url) -> Self {
        Builder {
            base: Some(url),
            ..Default::default()
        }
    }
}

/// Percent-encoded URL parts.
///
/// Created by [`Url::into_parts`]; [`Url::from_parts`] validates and joins
/// the parts back.
#[derive(Debug, Default, Clone)]
pub struct Parts {
    /// Scheme without the `:`.
    pub scheme: Option<ByteString>,
    /// Authority without the leading `//`.
    pub authority: Option<ByteString>,
    /// Path and query.
    pub path_and_query: ByteString,
    /// Fragment without the `#`.
    pub fragment: Option<ByteString>,
}

impl Url {
    /// Creates a URL from strictly validated parts.
    ///
    /// ```
    /// use urly::{Parts, Url};
    ///
    /// let url = Url::from_static("http://example.com/a?b#c");
    /// let mut parts = url.into_parts();
    /// assert_eq!(parts.path_and_query, "/a?b");
    /// parts.fragment = None;
    /// assert_eq!(Url::from_parts(parts).unwrap(), "http://example.com/a?b");
    ///
    /// let parts = Parts { path_and_query: "a b".into(), ..Parts::default() };
    /// assert!(Url::from_parts(parts).is_err());
    /// ```
    pub fn from_parts(parts: Parts) -> Result<Url, InvalidUrlParts> {
        let invalid_path = || InvalidUrlParts::from(InvalidUrl::new(ErrorKind::InvalidPath));

        let pq = PathAndQuery::new(&parts.path_and_query)?;
        let path = pq.path().as_str();
        if let Some(scheme) = &parts.scheme {
            Scheme::new(scheme)?;
        }
        if let Some(authority) = &parts.authority {
            Authority::new(authority)?;
            if !path.is_empty() && !path.starts_with('/') {
                return Err(invalid_path());
            }
        } else {
            if path.starts_with("//") {
                return Err(invalid_path());
            }
            if parts.scheme.is_none() && path.split('/').next().unwrap_or("").contains(':') {
                return Err(invalid_path());
            }
        }
        if let Some(fragment) = &parts.fragment {
            Fragment::new(fragment)?;
        }

        let mut s = String::new();
        if let Some(scheme) = &parts.scheme {
            s.push_str(scheme);
            s.push(':');
        }
        if let Some(authority) = &parts.authority {
            s.push_str("//");
            s.push_str(authority);
        }
        s.push_str(&parts.path_and_query);
        if let Some(fragment) = &parts.fragment {
            s.push('#');
            s.push_str(fragment);
        }
        if s.is_empty() {
            return Err(InvalidUrl::new(ErrorKind::Empty).into());
        }
        Ok(parse::parse(&s, None)?)
    }
}
