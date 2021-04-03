use std::{convert::TryFrom, error::Error, fmt, net, rc::Rc, time::Duration};

use bytes::Bytes;
use futures_core::Stream;
use serde::Serialize;

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use crate::http::body::Body;
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::{
    uri, ConnectionType, Method, RequestHead, RequestHeadType, Uri, Version,
};

use super::error::{FreezeRequestError, InvalidUrl};
use super::frozen::FrozenClientRequest;
use super::sender::{PrepForSendingError, SendClientRequest};
use super::ClientConfig;

#[cfg(feature = "compress")]
const HTTPS_ENCODING: &str = "br, gzip, deflate";
#[cfg(not(feature = "compress"))]
const HTTPS_ENCODING: &str = "br";

/// An HTTP Client request builder
///
/// This type can be used to construct an instance of `ClientRequest` through a
/// builder-like pattern.
///
/// ```rust
/// use ntex::http::client::Client;
///
/// #[ntex::main]
/// async fn main() {
///    let response = Client::new()
///         .get("http://www.rust-lang.org") // <- Create request builder
///         .header("User-Agent", "ntex::web")
///         .send()                          // <- Send http request
///         .await;
///
///    response.and_then(|response| {   // <- server http response
///         println!("Response: {:?}", response);
///         Ok(())
///    });
/// }
/// ```
pub struct ClientRequest {
    pub(crate) head: RequestHead,
    err: Option<HttpError>,
    addr: Option<net::SocketAddr>,
    #[cfg(feature = "cookie")]
    cookies: Option<CookieJar>,
    response_decompress: bool,
    timeout: Option<Duration>,
    config: Rc<ClientConfig>,
}

impl ClientRequest {
    /// Create new client request builder.
    pub(super) fn new<U>(method: Method, uri: U, config: Rc<ClientConfig>) -> Self
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        ClientRequest {
            config,
            head: RequestHead::default(),
            err: None,
            addr: None,
            #[cfg(feature = "cookie")]
            cookies: None,
            timeout: None,
            response_decompress: true,
        }
        .method(method)
        .uri(uri)
    }

    /// Set HTTP URI of request.
    #[inline]
    pub fn uri<U>(mut self, uri: U) -> Self
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        match Uri::try_from(uri) {
            Ok(uri) => self.head.uri = uri,
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Get HTTP URI of request.
    pub fn get_uri(&self) -> &Uri {
        &self.head.uri
    }

    /// Set socket address of the server.
    ///
    /// This address is used for connection. If address is not
    /// provided url's host name get resolved.
    pub fn address(mut self, addr: net::SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Set HTTP method of this request.
    #[inline]
    pub fn method(mut self, method: Method) -> Self {
        self.head.method = method;
        self
    }

    /// Get HTTP method of this request
    pub fn get_method(&self) -> &Method {
        &self.head.method
    }

    #[doc(hidden)]
    /// Set HTTP version of this request.
    ///
    /// By default requests's HTTP version depends on network stream
    #[inline]
    pub fn version(mut self, version: Version) -> Self {
        self.head.version = version;
        self
    }

    /// Get HTTP version of this request.
    pub fn get_version(&self) -> &Version {
        &self.head.version
    }

    #[inline]
    /// Returns request's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head.headers
    }

    #[inline]
    /// Returns request's mutable headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head.headers
    }

    /// Append a header.
    ///
    /// Header gets appended to existing header.
    /// To override header use `set_header()` method.
    ///
    /// ```rust
    /// use ntex::http;
    /// use ntex::http::client::Client;
    ///
    /// #[ntex::main]
    /// async fn main() {
    ///     let req = Client::new()
    ///         .get("http://www.rust-lang.org")
    ///         .header("X-TEST", "value")
    ///         .header(http::header::CONTENT_TYPE, "application/json");
    /// }
    /// ```
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => self.head.headers.append(key, value),
                Err(e) => self.err = Some(e.into()),
            },
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Insert a header, replaces existing header.
    pub fn set_header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => self.head.headers.insert(key, value),
                Err(e) => self.err = Some(e.into()),
            },
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Insert a header only if it is not yet set.
    pub fn set_header_if_none<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => {
                if !self.head.headers.contains_key(&key) {
                    match HeaderValue::try_from(value) {
                        Ok(value) => self.head.headers.insert(key, value),
                        Err(e) => self.err = Some(e.into()),
                    }
                }
            }
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Force close connection instead of returning it back to connections pool.
    /// This setting affect only http/1 connections.
    #[inline]
    pub fn force_close(mut self) -> Self {
        self.head.set_connection_type(ConnectionType::Close);
        self
    }

    /// Set request's content type
    #[inline]
    pub fn content_type<V>(mut self, value: V) -> Self
    where
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderValue::try_from(value) {
            Ok(value) => self.head.headers.insert(header::CONTENT_TYPE, value),
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Set content length
    #[inline]
    pub fn content_length(self, len: u64) -> Self {
        self.header(header::CONTENT_LENGTH, len)
    }

    /// Set HTTP basic authorization header
    pub fn basic_auth<U>(self, username: U, password: Option<&str>) -> Self
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{}:{}", username, password),
            None => format!("{}:", username),
        };
        self.header(
            header::AUTHORIZATION,
            format!("Basic {}", base64::encode(&auth)),
        )
    }

    /// Set HTTP bearer authentication header
    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: fmt::Display,
    {
        self.header(header::AUTHORIZATION, format!("Bearer {}", token))
    }

    #[cfg(feature = "cookie")]
    /// Set a cookie
    ///
    /// ```rust
    /// use coo_kie as cookie;
    /// use ntex::http::client::Client;
    ///
    /// #[ntex::main]
    /// async fn main() {
    ///     let resp = Client::new().get("https://www.rust-lang.org")
    ///         .cookie(
    ///             cookie::Cookie::build("name", "value")
    ///                 .domain("www.rust-lang.org")
    ///                 .path("/")
    ///                 .secure(true)
    ///                 .http_only(true)
    ///                 .finish(),
    ///          )
    ///          .send()
    ///          .await;
    ///
    ///     println!("Response: {:?}", resp);
    /// }
    /// ```
    pub fn cookie(mut self, cookie: Cookie<'_>) -> Self {
        if self.cookies.is_none() {
            let mut jar = CookieJar::new();
            jar.add(cookie.into_owned());
            self.cookies = Some(jar)
        } else {
            self.cookies.as_mut().unwrap().add(cookie.into_owned());
        }
        self
    }

    /// Disable automatic decompress of response's body
    pub fn no_decompress(mut self) -> Self {
        self.response_decompress = false;
        self
    }

    /// Set request timeout. Overrides client wide timeout setting.
    ///
    /// Request timeout is the total time before a response must be received.
    /// Default value is 5 seconds.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// This method calls provided closure with builder reference if
    /// value is `true`.
    pub fn if_true<F>(self, value: bool, f: F) -> Self
    where
        F: FnOnce(ClientRequest) -> ClientRequest,
    {
        if value {
            f(self)
        } else {
            self
        }
    }

    /// This method calls provided closure with builder reference if
    /// value is `Some`.
    pub fn if_some<T, F>(self, value: Option<T>, f: F) -> Self
    where
        F: FnOnce(T, ClientRequest) -> ClientRequest,
    {
        if let Some(val) = value {
            f(val, self)
        } else {
            self
        }
    }

    /// Sets the query part of the request
    pub fn query<T: Serialize>(
        mut self,
        query: &T,
    ) -> Result<Self, serde_urlencoded::ser::Error> {
        let mut parts = self.head.uri.clone().into_parts();

        if let Some(path_and_query) = parts.path_and_query {
            let query = serde_urlencoded::to_string(query)?;
            let path = path_and_query.path();
            parts.path_and_query = format!("{}?{}", path, query).parse().ok();

            match Uri::from_parts(parts) {
                Ok(uri) => self.head.uri = uri,
                Err(e) => self.err = Some(e.into()),
            }
        }

        Ok(self)
    }

    /// Freeze request builder and construct `FrozenClientRequest`,
    /// which could be used for sending same request multiple times.
    pub fn freeze(self) -> Result<FrozenClientRequest, FreezeRequestError> {
        let slf = match self.prep_for_sending() {
            Ok(slf) => slf,
            Err(e) => return Err(e.into()),
        };

        let request = FrozenClientRequest {
            head: Rc::new(slf.head),
            addr: slf.addr,
            response_decompress: slf.response_decompress,
            timeout: slf.timeout,
            config: slf.config,
        };

        Ok(request)
    }

    /// Complete request construction and send body.
    pub fn send_body<B>(self, body: B) -> SendClientRequest
    where
        B: Into<Body>,
    {
        let slf = match self.prep_for_sending() {
            Ok(slf) => slf,
            Err(e) => return e.into(),
        };

        RequestHeadType::Owned(slf.head).send_body(
            slf.addr,
            slf.response_decompress,
            slf.timeout,
            slf.config.as_ref(),
            body,
        )
    }

    /// Set a JSON body and generate `ClientRequest`
    pub fn send_json<T: Serialize>(self, value: &T) -> SendClientRequest {
        let slf = match self.prep_for_sending() {
            Ok(slf) => slf,
            Err(e) => return e.into(),
        };

        RequestHeadType::Owned(slf.head).send_json(
            slf.addr,
            slf.response_decompress,
            slf.timeout,
            slf.config.as_ref(),
            value,
        )
    }

    /// Set a urlencoded body and generate `ClientRequest`
    ///
    /// `ClientRequestBuilder` can not be used after this call.
    pub fn send_form<T: Serialize>(self, value: &T) -> SendClientRequest {
        let slf = match self.prep_for_sending() {
            Ok(slf) => slf,
            Err(e) => return e.into(),
        };

        RequestHeadType::Owned(slf.head).send_form(
            slf.addr,
            slf.response_decompress,
            slf.timeout,
            slf.config.as_ref(),
            value,
        )
    }

    /// Set an streaming body and generate `ClientRequest`.
    pub fn send_stream<S, E>(self, stream: S) -> SendClientRequest
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        let slf = match self.prep_for_sending() {
            Ok(slf) => slf,
            Err(e) => return e.into(),
        };

        RequestHeadType::Owned(slf.head).send_stream(
            slf.addr,
            slf.response_decompress,
            slf.timeout,
            slf.config.as_ref(),
            stream,
        )
    }

    /// Set an empty body and generate `ClientRequest`.
    pub fn send(self) -> SendClientRequest {
        let slf = match self.prep_for_sending() {
            Ok(slf) => slf,
            Err(e) => return e.into(),
        };

        RequestHeadType::Owned(slf.head).send(
            slf.addr,
            slf.response_decompress,
            slf.timeout,
            slf.config.as_ref(),
        )
    }

    #[allow(unused_mut)]
    fn prep_for_sending(mut self) -> Result<Self, PrepForSendingError> {
        if let Some(e) = self.err {
            return Err(e.into());
        }

        // validate uri
        let uri = &self.head.uri;
        if uri.host().is_none() {
            return Err(InvalidUrl::MissingHost.into());
        } else if uri.scheme().is_none() {
            return Err(InvalidUrl::MissingScheme.into());
        } else if let Some(scheme) = uri.scheme() {
            match scheme.as_str() {
                "http" | "ws" | "https" | "wss" => (),
                _ => return Err(InvalidUrl::UnknownScheme.into()),
            }
        } else {
            return Err(InvalidUrl::UnknownScheme.into());
        }

        // set cookies
        #[cfg(feature = "cookie")]
        {
            use percent_encoding::percent_encode;
            use std::fmt::Write as FmtWrite;

            if let Some(ref mut jar) = self.cookies {
                let mut cookie = String::new();
                for c in jar.delta() {
                    let name = percent_encode(
                        c.name().as_bytes(),
                        crate::http::helpers::USERINFO,
                    );
                    let value = percent_encode(
                        c.value().as_bytes(),
                        crate::http::helpers::USERINFO,
                    );
                    let _ = write!(&mut cookie, "; {}={}", name, value);
                }
                self.head.headers.insert(
                    header::COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
            }
        }

        let mut slf = self;

        if slf.response_decompress {
            let https = slf
                .head
                .uri
                .scheme()
                .map(|s| s == &uri::Scheme::HTTPS)
                .unwrap_or(true);

            if https {
                slf = slf.set_header_if_none(header::ACCEPT_ENCODING, HTTPS_ENCODING)
            } else {
                #[cfg(any(feature = "compress"))]
                {
                    slf =
                        slf.set_header_if_none(header::ACCEPT_ENCODING, "gzip, deflate")
                }
            };
        }

        Ok(slf)
    }
}

impl fmt::Debug for ClientRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nClientRequest {:?} {}:{}",
            self.head.version, self.head.method, self.head.uri
        )?;
        writeln!(f, "  headers:")?;
        for (key, val) in self.head.headers.iter() {
            if key == header::AUTHORIZATION {
                writeln!(f, "    {:?}: <REDACTED>", key)?;
            } else {
                writeln!(f, "    {:?}: {:?}", key, val)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::client::Client;

    #[crate::rt_test]
    async fn test_debug() {
        let request = Client::new().get("/").header("x-test", "111");
        let repr = format!("{:?}", request);
        assert!(repr.contains("ClientRequest"));
        assert!(repr.contains("x-test"));
    }

    #[crate::rt_test]
    async fn test_basics() {
        let mut req = Client::new()
            .put("/")
            .version(Version::HTTP_2)
            .header(header::DATE, "data")
            .content_type("plain/text")
            .if_true(true, |req| req.header(header::SERVER, "awc"))
            .if_true(false, |req| req.header(header::EXPECT, "awc"))
            .if_some(Some("server"), |val, req| {
                req.header(header::USER_AGENT, val)
            })
            .if_some(Option::<&str>::None, |_, req| {
                req.header(header::ALLOW, "1")
            })
            .content_length(100);
        assert!(req.headers().contains_key(header::CONTENT_TYPE));
        assert!(req.headers().contains_key(header::DATE));
        assert!(req.headers().contains_key(header::SERVER));
        assert!(req.headers().contains_key(header::USER_AGENT));
        assert!(!req.headers().contains_key(header::ALLOW));
        assert!(!req.headers().contains_key(header::EXPECT));
        assert_eq!(req.head.version, Version::HTTP_2);
        assert_eq!(req.get_version(), &Version::HTTP_2);
        assert_eq!(req.get_method(), Method::PUT);
        let _ = req.headers_mut();
        let _ = req.send_body("");
    }

    #[crate::rt_test]
    async fn test_client_header() {
        let req = Client::build()
            .header(header::CONTENT_TYPE, "111")
            .finish()
            .get("/");

        assert_eq!(
            req.head
                .headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "111"
        );
    }

    #[crate::rt_test]
    async fn test_client_header_override() {
        let req = Client::build()
            .header(header::CONTENT_TYPE, "111")
            .finish()
            .get("/")
            .set_header(header::CONTENT_TYPE, "222");

        assert_eq!(
            req.head
                .headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "222"
        );
    }

    #[crate::rt_test]
    async fn client_basic_auth() {
        let req = Client::new()
            .get("/")
            .basic_auth("username", Some("password"));
        assert_eq!(
            req.head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );

        let req = Client::new().get("/").basic_auth("username", None);
        assert_eq!(
            req.head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6"
        );
    }

    #[crate::rt_test]
    async fn client_bearer_auth() {
        let req = Client::new().get("/").bearer_auth("someS3cr3tAutht0k3n");
        assert_eq!(
            req.head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k3n"
        );
    }

    #[crate::rt_test]
    async fn client_query() {
        let req = Client::new()
            .get("/")
            .query(&[("key1", "val1"), ("key2", "val2")])
            .unwrap();
        assert_eq!(req.get_uri().query().unwrap(), "key1=val1&key2=val2");
    }
}
