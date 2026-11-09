use std::{error::Error as StdError, fmt, net, rc::Rc};

use base64::{Engine, engine::general_purpose::STANDARD as base64};
#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};
use serde::Serialize;
use urly::Url;

use crate::error::Error;
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::{ConnectionType, Method, Version, body::Body};
use crate::{Cfg, PipelineBinding, time::Millis, util::Bytes, util::Stream};

use super::error::{ClientError, InvalidUrl};
use super::{ClientConfig, ClientResponse, ServiceRequest, ServiceResponse};

/// An HTTP client request builder.
///
/// Builder methods configure the request and the `send*` methods consume it,
/// send the request, and return a [`ClientResponse`].
///
/// ```rust
/// use ntex::client::Client;
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
    request: ServiceRequest,
    svc: PipelineBinding<ServiceRequest, ServiceResponse, Error<ClientError>>,
    err: Option<ClientError>,
    cfg: Cfg<ClientConfig>,
    #[cfg(feature = "cookie")]
    cookies: Option<CookieJar>,
}

impl ClientRequest {
    /// Create new client request builder.
    pub(super) fn new<U>(
        method: Method,
        uri: U,
        cfg: Cfg<ClientConfig>,
        svc: PipelineBinding<ServiceRequest, ServiceResponse, Error<ClientError>>,
    ) -> Self
    where
        Url: TryFrom<U>,
        <Url as TryFrom<U>>::Error: Into<InvalidUrl>,
    {
        ClientRequest {
            svc,
            cfg,
            request: ServiceRequest::new(),
            err: None,
            #[cfg(feature = "cookie")]
            cookies: None,
        }
        .method(method)
        .uri(uri)
    }

    /// Sets the request URI.
    ///
    /// URI conversion errors are stored and returned by the next `send*` call.
    #[inline]
    #[must_use]
    pub fn uri<U>(mut self, uri: U) -> Self
    where
        Url: TryFrom<U>,
        <Url as TryFrom<U>>::Error: Into<InvalidUrl>,
    {
        match Url::try_from(uri) {
            Ok(uri) => self.request.head.uri = uri,
            Err(e) => self.err = Some(e.into().into()),
        }
        self
    }

    /// Returns the request URI.
    pub fn get_uri(&self) -> &Url {
        &self.request.head.uri
    }

    #[must_use]
    /// Sets the server socket address.
    ///
    /// This address is used instead of resolving the URI host name. Pooled
    /// connections are reused only by requests with the same URI authority
    /// and the same address, so requests to one host with different
    /// addresses, or without one, never share a connection.
    pub fn address(mut self, addr: net::SocketAddr) -> Self {
        self.request.addr = Some(addr);
        self
    }

    /// Sets the request method.
    #[inline]
    #[must_use]
    pub fn method(mut self, method: Method) -> Self {
        self.request.head.method = method;
        self
    }

    #[inline]
    #[must_use]
    /// Returns the request method.
    pub fn get_method(&self) -> &Method {
        &self.request.head.method
    }

    /// Sets the request HTTP version.
    ///
    /// The version does not select the transport protocol. On an HTTP/1
    /// connection, `HTTP/1.0` is sent as requested and any other version is
    /// sent as `HTTP/1.1`. An HTTP/2 connection always uses `HTTP/2`.
    ///
    /// An `HTTP/1.0` request closes the connection after the response unless
    /// keep-alive is requested, and a streaming body needs a `Content-Length`
    /// header because chunked coding is not available.
    #[inline]
    #[must_use]
    pub fn version(mut self, version: Version) -> Self {
        self.request.head.version = version;
        self
    }

    #[inline]
    /// Returns the request HTTP version.
    pub fn get_version(&self) -> &Version {
        &self.request.head.version
    }

    #[inline]
    /// Returns the request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.request.head.headers
    }

    #[inline]
    /// Returns mutable access to the request headers.
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.request.head.headers
    }

    #[must_use]
    /// Append a header.
    ///
    /// The header is appended to any existing values with the same name. Use
    /// [`set_header`](Self::set_header) to replace existing values.
    ///
    /// Header conversion errors are stored and returned by the next `send*`
    /// call.
    ///
    /// ```rust
    /// use ntex::{http, client::Client};
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
                Ok(value) => self.request.head.headers.append(key, value),
                Err(e) => self.err = Some(ClientError::Http(e.into())),
            },
            Err(e) => self.err = Some(ClientError::Http(e.into())),
        }
        self
    }

    #[must_use]
    /// Inserts a header, replacing existing values with the same name.
    ///
    /// Header conversion errors are stored and returned by the next `send*`
    /// call.
    pub fn set_header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => self.request.head.headers.insert(key, value),
                Err(e) => self.err = Some(ClientError::Http(e.into())),
            },
            Err(e) => self.err = Some(ClientError::Http(e.into())),
        }
        self
    }

    #[must_use]
    /// Inserts a header if the request does not already contain one with the
    /// same name.
    ///
    /// Header conversion errors are stored and returned by the next `send*`
    /// call.
    pub fn set_header_if_none<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => {
                if !self.request.head.headers.contains_key(&key) {
                    match HeaderValue::try_from(value) {
                        Ok(value) => self.request.head.headers.insert(key, value),
                        Err(e) => self.err = Some(ClientError::Http(e.into())),
                    }
                }
            }
            Err(e) => self.err = Some(ClientError::Http(e.into())),
        }
        self
    }

    #[inline]
    #[must_use]
    /// Sets the request connection type.
    ///
    /// See [`RequestHead::set_connection_type()`](crate::http::RequestHead::set_connection_type)
    /// for how repeated calls are resolved. On HTTP/1 connections, the `close`,
    /// `keep-alive` and `upgrade` options of a `Connection` header set with
    /// [`header`](Self::header) are applied the same way. HTTP/2 connections
    /// ignore the connection type.
    pub fn set_connection_type(mut self, ctype: ConnectionType) -> Self {
        self.request.head.set_connection_type(ctype);
        self
    }

    /// Prevents an HTTP/1 connection from returning to the connection pool.
    ///
    /// This setting affects only HTTP/1 connections.
    #[inline]
    #[must_use]
    pub fn force_close(mut self) -> Self {
        self.request.head.set_connection_type(ConnectionType::Close);
        self
    }

    /// Sets the request content type.
    ///
    /// Header conversion errors are stored and returned by the next `send*`
    /// call.
    #[inline]
    #[must_use]
    pub fn content_type<V>(mut self, value: V) -> Self
    where
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderValue::try_from(value) {
            Ok(value) => self
                .request
                .head
                .headers
                .insert(header::CONTENT_TYPE, value),
            Err(e) => self.err = Some(ClientError::Http(e.into())),
        }
        self
    }

    /// Sets the request content length.
    ///
    /// A streaming body (see [`send_stream`](Self::send_stream)) is sent with
    /// this length instead of chunked coding on HTTP/1 connections. Sending
    /// fails if the stream ends early or produces more data. A body with a known
    /// size is sent with its actual size and this value is ignored.
    #[inline]
    #[must_use]
    pub fn content_length(self, len: u64) -> Self {
        self.set_header(header::CONTENT_LENGTH, len)
    }

    #[must_use]
    /// Sets the HTTP basic authentication header.
    ///
    /// Replaces any existing `Authorization` header, including a client default.
    pub fn basic_auth<U>(self, username: U, password: Option<&str>) -> Self
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.set_header(
            header::AUTHORIZATION,
            format!("Basic {}", base64.encode(auth)),
        )
    }

    #[must_use]
    /// Sets the HTTP bearer authentication header.
    ///
    /// Replaces any existing `Authorization` header, including a client default.
    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: fmt::Display,
    {
        self.set_header(header::AUTHORIZATION, format!("Bearer {token}"))
    }

    #[must_use]
    #[cfg(feature = "cookie")]
    /// Set a cookie.
    ///
    /// When the request is sent, all cookies and every raw `Cookie` header
    /// value are merged into a single `Cookie` header.
    ///
    /// ```rust
    /// use coo_kie as cookie;
    /// use ntex::client::Client;
    ///
    /// #[ntex::main]
    /// async fn main() {
    ///     let resp = Client::new().get("https://www.rust-lang.org")
    ///         .cookie(
    ///             cookie::Cookie::build(("name", "value"))
    ///                 .domain("www.rust-lang.org")
    ///                 .path("/")
    ///                 .secure(true)
    ///                 .http_only(true)
    ///          )
    ///          .send()
    ///          .await;
    ///
    ///     println!("Response: {:?}", resp);
    /// }
    /// ```
    pub fn cookie<C>(mut self, cookie: C) -> Self
    where
        C: Into<Cookie<'static>>,
    {
        if let Some(cookies) = &mut self.cookies {
            cookies.add(cookie.into());
        } else {
            let mut jar = CookieJar::new();
            jar.add(cookie.into());
            self.cookies = Some(jar);
        }
        self
    }

    #[must_use]
    /// Disables automatic decompression of the response body.
    pub fn no_decompress(mut self) -> Self {
        self.request.response_decompress = false;
        self
    }

    #[must_use]
    /// Sets the response-header timeout for this request.
    ///
    /// This overrides the client-wide timeout. The timeout covers receiving the
    /// response head after the request has been sent. It does not cover waiting
    /// for a pooled connection, connecting, sending the request body, or
    /// reading the response body; use
    /// [`MessageBody::timeout`](super::MessageBody::timeout) or
    /// [`JsonBody::timeout`](super::JsonBody::timeout) for the body.
    /// A zero duration disables only this timeout for this request.
    ///
    /// The client-wide default is 5 seconds.
    pub fn timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.request.timeout = Some(timeout.into());
        self
    }

    #[must_use]
    /// Applies `f` to this builder when `value` is `true`.
    pub fn if_true<F>(self, value: bool, f: F) -> Self
    where
        F: FnOnce(ClientRequest) -> ClientRequest,
    {
        if value { f(self) } else { self }
    }

    #[must_use]
    /// Applies `f` and the contained value to this builder when `value` is
    /// [`Some`].
    pub fn if_some<T, F>(self, value: Option<T>, f: F) -> Self
    where
        F: FnOnce(T, ClientRequest) -> ClientRequest,
    {
        if let Some(val) = value { f(val, self) } else { self }
    }

    /// Serializes `query` and replaces the query component of the request URI.
    ///
    /// Serialization errors, and a resulting URI longer than 65535 bytes, are
    /// stored and returned by the next `send*` call.
    #[must_use]
    pub fn query<T: Serialize>(mut self, query: &T) -> Self {
        let query = match serde_urlencoded::to_string(query) {
            Ok(query) => query,
            Err(err) => {
                self.err = Some(ClientError::Error(Rc::new(err)));
                return self;
            }
        };

        match urly::Builder::from(self.request.head.uri.clone())
            .query(&query)
            .build()
        {
            Ok(uri) => self.request.head.uri = uri,
            Err(e) => self.err = Some(InvalidUrl::Parse(e).into()),
        }
        self
    }
}

impl ClientRequest {
    /// Sends the request with `body`.
    pub async fn send_body<B>(mut self, body: B) -> Result<ClientResponse, Error<ClientError>>
    where
        B: Into<Body>,
    {
        self.prep_for_sending()?;
        *self.request.body() = body.into();
        self.svc.call(self.request).await.map(Into::into)
    }

    /// Serializes `value` as JSON and sends the request.
    pub async fn send_json<T: Serialize>(
        mut self,
        value: &T,
    ) -> Result<ClientResponse, Error<ClientError>> {
        self.prep_for_sending()?;
        self.request.set_json(value)?;
        self.svc.call(self.request).await.map(Into::into)
    }

    /// Serializes `value` as a URL-encoded form and sends the request.
    pub async fn send_form<T: Serialize>(
        mut self,
        value: &T,
    ) -> Result<ClientResponse, Error<ClientError>> {
        self.prep_for_sending()?;
        self.request.set_form(value)?;
        self.svc.call(self.request).await.map(Into::into)
    }

    /// Sends the request with a streaming body.
    pub async fn send_stream<T, E>(
        mut self,
        stream: T,
    ) -> Result<ClientResponse, Error<ClientError>>
    where
        T: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: StdError + 'static,
    {
        self.prep_for_sending()?;
        self.request.set_stream(stream);
        self.svc.call(self.request).await.map(Into::into)
    }

    /// Sends the request with an empty body.
    pub async fn send(mut self) -> Result<ClientResponse, Error<ClientError>> {
        self.prep_for_sending()?;
        self.svc.call(self.request).await.map(Into::into)
    }

    fn prep_for_sending(&mut self) -> Result<(), Error<ClientError>> {
        self.prep_for_sending_inner()
            .map_err(|e| e.with_service(self.cfg.service()))
    }

    fn prep_for_sending_inner(&mut self) -> Result<(), Error<ClientError>> {
        if let Some(e) = self.err.take() {
            return Err(e.into());
        }

        // validate uri
        let uri = &self.request.head.uri;
        if uri.host().is_none() {
            return Err(ClientError::from(InvalidUrl::MissingHost).into());
        }
        match uri.scheme_str() {
            Some("http" | "ws" | "https" | "wss") => (),
            Some(_) => return Err(ClientError::from(InvalidUrl::UnknownScheme).into()),
            None => return Err(ClientError::from(InvalidUrl::MissingScheme).into()),
        }

        // set cookies, merged with all existing `Cookie` headers into a single header
        #[cfg(feature = "cookie")]
        {
            if let Some(ref jar) = self.cookies {
                let headers = &mut self.request.head.headers;
                let mut cookie = Vec::new();
                for v in headers.get_all(header::COOKIE) {
                    if !v.is_empty() {
                        if !cookie.is_empty() {
                            cookie.extend_from_slice(b"; ");
                        }
                        cookie.extend_from_slice(v.as_bytes());
                    }
                }
                for c in jar.iter() {
                    crate::http::helpers::push_cookie(&mut cookie, c.name(), c.value());
                }
                if let Ok(val) = HeaderValue::from_bytes(&cookie) {
                    headers.insert(header::COOKIE, val);
                }
            }
        }

        #[cfg(feature = "compress")]
        if self.request.response_decompress
            && !self
                .request
                .head
                .headers
                .contains_key(&header::ACCEPT_ENCODING)
        {
            const COMPRESSION: HeaderValue = HeaderValue::from_static("gzip, deflate, zstd");
            self.request
                .head
                .headers
                .insert(header::ACCEPT_ENCODING, COMPRESSION);
        }

        Ok(())
    }
}

impl fmt::Debug for ClientRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nClientRequest {:?} {}:{}",
            self.request.head.version, self.request.head.method, self.request.head.uri
        )?;
        writeln!(f, "  headers:")?;
        for (key, val) in &self.request.head.headers {
            if key == header::AUTHORIZATION {
                writeln!(f, "    {key:?}: <REDACTED>")?;
            } else {
                writeln!(f, "    {key:?}: {val:?}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SharedCfg, client::Client};

    struct InvalidQuery;

    impl Serialize for InvalidQuery {
        fn serialize<S>(&self, _: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("invalid query"))
        }
    }

    /// STATEMENT: Request debug output identifies the request and its ordinary headers.
    ///
    /// HOW:
    /// - Format a request carrying a custom header and check both the type name and header name appear.
    #[crate::rt_test]
    async fn test_debug() {
        let request = Client::new().get("/").header("x-test", "111");
        let repr = format!("{request:?}");
        assert!(repr.contains("ClientRequest"));
        assert!(repr.contains("x-test"));
    }

    /// STATEMENT: Request builders apply method, version, headers, and conditional transformations.
    ///
    /// HOW:
    /// - Build an HTTP/2 PUT with true/false and present/absent conditions and inspect which headers were added.
    /// - Check method/version accessors and exercise mutable headers and body sending.
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
        assert_eq!(req.request.head.version, Version::HTTP_2);
        assert_eq!(req.get_version(), &Version::HTTP_2);
        assert_eq!(req.get_method(), Method::PUT);
        let _ = req.headers_mut();
        let _ = req.send_body("").await;
    }

    /// STATEMENT: Request cookies combine with one another and with every preexisting `Cookie` header into a single header.
    ///
    /// HOW:
    /// - Prepare requests with two builder cookies and with a raw header plus one cookie, then compare header entries.
    /// - Add three raw `Cookie` headers, one empty, plus a builder cookie, and check that a single header joins all non-empty values in order.
    #[cfg(feature = "cookie")]
    #[crate::rt_test]
    async fn cookies_extend_cookie_header() {
        use coo_kie::Cookie;

        fn cookie_header(req: &mut ClientRequest) -> String {
            req.prep_for_sending_inner().unwrap();
            let val = req.request.head.headers.get(header::COOKIE).unwrap();
            val.to_str().unwrap().to_string()
        }

        let mut req = Client::new()
            .get("http://localhost/")
            .cookie(Cookie::build(("c1", "v1")))
            .cookie(Cookie::build(("c2", "v2")));
        let cookie = cookie_header(&mut req);
        let mut cookies: Vec<_> = cookie.split("; ").collect();
        cookies.sort_unstable();
        assert_eq!(cookies, ["c1=v1", "c2=v2"]);

        let mut req = Client::new()
            .get("http://localhost/")
            .header(header::COOKIE, "c0=v0")
            .cookie(Cookie::build(("c1", "v1")));
        assert_eq!(cookie_header(&mut req), "c0=v0; c1=v1");

        let mut req = Client::new()
            .get("http://localhost/")
            .header(header::COOKIE, "a=1")
            .header(header::COOKIE, "")
            .header(header::COOKIE, "b=2; c=3")
            .cookie(Cookie::build(("c1", "v1")));
        assert_eq!(cookie_header(&mut req), "a=1; b=2; c=3; c1=v1");
        assert_eq!(req.headers().get_all(header::COOKIE).count(), 1);
    }

    /// STATEMENT: Every value of a multi-value client default header is copied into new requests.
    ///
    /// HOW:
    /// - Configure two default `Accept` values and check that a new request carries both, in order.
    /// - Replace them with `set_header` and check that only the request value remains.
    #[crate::rt_test]
    async fn test_client_multi_value_default_header() {
        let client = Client::builder().build(
            SharedCfg::new("H").add(
                ClientConfig::new()
                    .set_header(header::ACCEPT, "text/html")
                    .unwrap()
                    .set_header(header::ACCEPT, "text/plain")
                    .unwrap(),
            ),
        );
        let req = client.get("/");
        let accept: Vec<_> = req.headers().get_all(header::ACCEPT).collect();
        assert_eq!(accept, ["text/html", "text/plain"]);

        let req = client.get("/").set_header(header::ACCEPT, "*/*");
        let accept: Vec<_> = req.headers().get_all(header::ACCEPT).collect();
        assert_eq!(accept, ["*/*"]);
    }

    /// STATEMENT: Client-wide default headers are copied into new requests.
    ///
    /// HOW:
    /// - Configure a default content type, construct a request, and compare its stored header.
    #[crate::rt_test]
    async fn test_client_header() {
        let req = Client::builder()
            .build(
                SharedCfg::new("H").add(
                    ClientConfig::new()
                        .set_header(header::CONTENT_TYPE, "111")
                        .unwrap(),
                ),
            )
            .get("/");

        assert_eq!(
            req.request
                .head
                .headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "111"
        );
    }

    /// STATEMENT: Request-specific header replacement overrides a client default.
    ///
    /// HOW:
    /// - Configure a default content type, replace it on a request, and check the new value.
    #[crate::rt_test]
    async fn test_client_header_override() {
        let req = Client::builder()
            .build(
                SharedCfg::new("H").add(
                    ClientConfig::new()
                        .set_header(header::CONTENT_TYPE, "111")
                        .unwrap(),
                ),
            )
            .get("/")
            .set_header(header::CONTENT_TYPE, "222");

        assert_eq!(
            req.request
                .head
                .headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "222"
        );
    }

    /// STATEMENT: Request Basic authentication supports both supplied and omitted passwords.
    ///
    /// HOW:
    /// - Build both credential forms and compare their complete encoded authorization values.
    #[crate::rt_test]
    async fn client_basic_auth() {
        let req = Client::new()
            .get("/")
            .basic_auth("username", Some("password"));
        assert_eq!(
            req.request
                .head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );

        let req = Client::new().get("/").basic_auth("username", None);
        assert_eq!(
            req.request
                .head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6"
        );
    }

    /// STATEMENT: Request bearer authentication constructs the expected authorization value.
    ///
    /// HOW:
    /// - Set a bearer token on a request and compare the resulting header.
    #[crate::rt_test]
    async fn client_bearer_auth() {
        let req = Client::new().get("/").bearer_auth("someS3cr3tAutht0k3n");
        assert_eq!(
            req.request
                .head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k3n"
        );
    }

    /// STATEMENT: Authentication and content-length setters replace prior values rather than append duplicates.
    ///
    /// HOW:
    /// - Override client authentication, repeat length setters, and switch authentication schemes.
    /// - Check resulting values and single-header counts for both requests and client configuration.
    #[crate::rt_test]
    async fn client_auth_replaces_header() {
        let client = Client::builder().build(
            SharedCfg::new("TEST").add(ClientConfig::new().set_bearer_auth("token").unwrap()),
        );
        let req = client
            .get("/")
            .basic_auth("username", Some("password"))
            .content_length(1)
            .content_length(2);
        let headers = &req.request.head.headers;
        let auth: Vec<_> = headers
            .get_all(header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(auth, ["Basic dXNlcm5hbWU6cGFzc3dvcmQ="]);
        let len: Vec<_> = headers
            .get_all(header::CONTENT_LENGTH)
            .map(|v| v.to_str().unwrap())
            .collect();
        assert_eq!(len, ["2"]);

        let req = Client::new()
            .get("/")
            .basic_auth("a", None)
            .bearer_auth("b");
        assert_eq!(
            req.request
                .head
                .headers
                .get_all(header::AUTHORIZATION)
                .count(),
            1
        );

        let cfg = ClientConfig::new()
            .set_basic_auth("a", None)
            .unwrap()
            .set_bearer_auth("b")
            .unwrap();
        assert_eq!(cfg.headers().get_all(header::AUTHORIZATION).count(), 1);
    }

    /// STATEMENT: Request construction errors are retained and returned when sending.
    ///
    /// HOW:
    /// - Build a malformed URL, inspect its stored parse error, and verify sending returns that category.
    /// - Build an invalid header and check it is stored as an HTTP error.
    #[crate::rt_test]
    async fn client_invalid_url() {
        let req = Client::new().get("http://local host/");
        assert!(matches!(
            req.err,
            Some(ClientError::Url(InvalidUrl::Parse(_)))
        ));
        let err = req.send().await.unwrap_err();
        assert!(matches!(
            err.into_error(),
            ClientError::Url(InvalidUrl::Parse(_))
        ));

        let req = Client::new().get("/").header("bad header", "1");
        assert!(matches!(req.err, Some(ClientError::Http(_))));
    }

    /// STATEMENT: Query serialization replaces an existing query while preserving the URL path and surfacing errors.
    ///
    /// HOW:
    /// - Serialize pairs into relative, pathless, and already-queried URLs and compare the results.
    /// - Use a failing serializer and check the request stores its error.
    #[crate::rt_test]
    async fn client_query() {
        let req = Client::new()
            .get("/")
            .query(&[("key1", "val1"), ("key2", "val2")]);
        assert_eq!(req.get_uri().query().unwrap(), "key1=val1&key2=val2");

        let req = Client::new().get("/").query(&InvalidQuery);
        assert!(matches!(req.err, Some(ClientError::Error(_))));

        // uri without path
        let req = Client::new().get("http://localhost").query(&[("k", "v")]);
        assert!(req.err.is_none());
        assert_eq!(req.get_uri(), "http://localhost/?k=v");

        // existing query is replaced, path is preserved
        let req = Client::new()
            .get("http://localhost/p?a=1")
            .query(&[("k", "v")]);
        assert_eq!(req.get_uri(), "http://localhost/p?k=v");
    }

    /// STATEMENT: A query that makes the URI too long is returned as an error by `send` instead of panicking.
    ///
    /// HOW:
    /// - Set a query value that makes `http://localhost/p?k=...` exactly 65535 bytes and check it is applied.
    /// - Set a value one byte longer and check the URI is unchanged and the stored error is `TooLong`.
    /// - Send the oversized request and check it fails with the same URL parse error.
    #[crate::rt_test]
    async fn client_query_too_long() {
        let prefix = "http://localhost/p?k=".len();
        let value = "a".repeat(65_535 - prefix);
        let req = Client::new()
            .get("http://localhost/p?a=1")
            .query(&[("k", &value)]);
        assert!(req.err.is_none());
        assert_eq!(req.get_uri().as_str().len(), 65_535);

        let value = "a".repeat(65_536 - prefix);
        let req = Client::new()
            .get("http://localhost/p?a=1")
            .query(&[("k", &value)]);
        assert_eq!(req.get_uri(), "http://localhost/p?a=1");
        assert!(matches!(
            &req.err,
            Some(ClientError::Url(InvalidUrl::Parse(e))) if e.kind() == urly::ErrorKind::TooLong
        ));
        let err = req.send().await.unwrap_err();
        assert!(matches!(
            err.into_error(),
            ClientError::Url(InvalidUrl::Parse(e)) if e.kind() == urly::ErrorKind::TooLong
        ));
    }

    /// STATEMENT: Header builders reject invalid input except when an existing conditional header makes replacement unnecessary.
    ///
    /// HOW:
    /// - Try malformed names and values across header setters and verify stored and send-time HTTP errors.
    /// - Attempt conditional replacement of an existing header with an invalid value and check the original survives.
    #[crate::rt_test]
    async fn client_invalid_headers() {
        let bad_value = "a\nb";
        for req in [
            Client::new().get("/").header("x-test", bad_value),
            Client::new().get("/").set_header("bad header", "1"),
            Client::new().get("/").set_header("x-test", bad_value),
            Client::new().get("/").set_header_if_none("bad header", "1"),
            Client::new()
                .get("/")
                .set_header_if_none("x-test", bad_value),
            Client::new().get("/").content_type(bad_value),
        ] {
            assert!(matches!(req.err, Some(ClientError::Http(_))), "{req:?}");
            let err = req.send().await.unwrap_err();
            assert!(matches!(err.into_error(), ClientError::Http(_)));
        }

        // an existing header is not replaced and its new value is not validated
        let req = Client::new()
            .get("/")
            .header("x-test", "1")
            .set_header_if_none("x-test", bad_value);
        assert!(req.err.is_none());
        assert_eq!(req.headers().get("x-test").unwrap(), "1");
    }

    /// STATEMENT: Sending rejects URLs with missing host, missing scheme, or unsupported scheme distinctly.
    ///
    /// HOW:
    /// - Send relative, scheme-relative, authority-like, and FTP URLs and compare their URL error classifications.
    #[crate::rt_test]
    async fn client_url_validation() {
        for (url, expected) in [
            ("/path", "missing-host"),
            ("//localhost:8080/", "missing-scheme"),
            ("localhost:8080", "missing-host"),
            ("ftp://localhost/", "unknown-scheme"),
        ] {
            let err = Client::new().get(url).send().await.unwrap_err();
            let kind = match err.into_error() {
                ClientError::Url(InvalidUrl::MissingHost) => "missing-host",
                ClientError::Url(InvalidUrl::MissingScheme) => "missing-scheme",
                ClientError::Url(InvalidUrl::UnknownScheme) => "unknown-scheme",
                err => panic!("{url}: {err:?}"),
            };
            assert_eq!(kind, expected, "{url}");
        }
    }

    /// STATEMENT: Request debug output redacts authorization while retaining an explicit connection address.
    ///
    /// HOW:
    /// - Build an authenticated request with an address override and check the address and redacted debug representation.
    #[crate::rt_test]
    async fn test_debug_redacts_authorization() {
        let req = Client::new()
            .get("http://localhost/")
            .basic_auth("user", Some("secret"))
            .address("127.0.0.1:1".parse().unwrap());
        assert_eq!(req.request.addr, Some("127.0.0.1:1".parse().unwrap()));
        let repr = format!("{req:?}");
        assert!(repr.contains("\"authorization\": <REDACTED>"), "{repr}");
        assert!(!repr.contains("Basic"), "{repr}");
    }
}
