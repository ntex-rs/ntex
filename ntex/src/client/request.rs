use std::{error::Error as StdError, fmt, net, rc::Rc};

use base64::{Engine, engine::general_purpose::STANDARD as base64};
#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};
use serde::Serialize;

use crate::error::Error;
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::{ConnectionType, Method, Uri, Version, body::Body};
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
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
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
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        match Uri::try_from(uri) {
            Ok(uri) => self.request.head.uri = uri,
            Err(e) => self.err = Some(ClientError::Http(e.into())),
        }
        self
    }

    /// Returns the request URI.
    pub fn get_uri(&self) -> &Uri {
        &self.request.head.uri
    }

    #[must_use]
    /// Sets the server socket address.
    ///
    /// This address is used instead of resolving the URI host name.
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
    /// By default, the version is selected from the negotiated transport
    /// protocol.
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
    /// for how repeated calls are resolved.
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
    #[inline]
    #[must_use]
    pub fn content_length(self, len: u64) -> Self {
        self.header(header::CONTENT_LENGTH, len)
    }

    #[must_use]
    /// Sets the HTTP basic authentication header.
    pub fn basic_auth<U>(self, username: U, password: Option<&str>) -> Self
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.header(
            header::AUTHORIZATION,
            format!("Basic {}", base64.encode(auth)),
        )
    }

    #[must_use]
    /// Sets the HTTP bearer authentication header.
    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: fmt::Display,
    {
        self.header(header::AUTHORIZATION, format!("Bearer {token}"))
    }

    #[must_use]
    #[cfg(feature = "cookie")]
    /// Set a cookie.
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
    /// response head after the request has been sent. A zero duration uses the
    /// client-wide timeout.
    ///
    /// The client-wide default is 5 seconds.
    pub fn timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.request.timeout = timeout.into();
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
    /// Serialization errors are stored and returned by the next `send*` call.
    #[must_use]
    pub fn query<T: Serialize>(mut self, query: &T) -> Self {
        let mut parts = self.request.head.uri.clone().into_parts();

        if let Some(path_and_query) = parts.path_and_query {
            let query = match serde_urlencoded::to_string(query) {
                Ok(query) => query,
                Err(err) => {
                    self.err = Some(ClientError::Error(Rc::new(err)));
                    return self;
                }
            };
            let path = path_and_query.path();
            parts.path_and_query = format!("{path}?{query}").parse().ok();

            match Uri::from_parts(parts) {
                Ok(uri) => self.request.head.uri = uri,
                Err(e) => self.err = Some(ClientError::Http(e.into())),
            }
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

    #[allow(unused_mut)]
    fn prep_for_sending(&mut self) -> Result<(), Error<ClientError>> {
        self.prep_for_sending_inner()
            .map_err(|e| e.set_service(self.cfg.service()))
    }

    #[allow(unused_mut)]
    fn prep_for_sending_inner(&mut self) -> Result<(), Error<ClientError>> {
        if let Some(e) = self.err.take() {
            return Err(e.into());
        }

        // validate uri
        let uri = &self.request.head.uri;
        {
            if uri.host().is_none() {
                Err(ClientError::from(InvalidUrl::MissingHost))
            } else if uri.scheme().is_none() {
                Err(ClientError::from(InvalidUrl::MissingScheme))
            } else if let Some(scheme) = uri.scheme() {
                if matches!(scheme.as_str(), "http" | "ws" | "https" | "wss") {
                    Ok(())
                } else {
                    Err(ClientError::from(InvalidUrl::UnknownScheme))
                }
            } else {
                Err(ClientError::from(InvalidUrl::UnknownScheme))
            }
        }?;

        // set cookies
        #[cfg(feature = "cookie")]
        {
            use percent_encoding::percent_encode;
            use std::fmt::Write as FmtWrite;

            if let Some(ref mut jar) = self.cookies {
                let mut cookie = String::new();
                for c in jar.delta() {
                    let name = percent_encode(c.name().as_bytes(), crate::http::helpers::USERINFO);
                    let value =
                        percent_encode(c.value().as_bytes(), crate::http::helpers::USERINFO);
                    let _ = write!(cookie, "; {name}={value}");
                }
                self.request.head.headers.insert(
                    header::COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
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
            const COMPRESSION: HeaderValue = HeaderValue::from_static("gzip, deflate");
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

    #[crate::rt_test]
    async fn test_debug() {
        let request = Client::new().get("/").header("x-test", "111");
        let repr = format!("{request:?}");
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
        assert_eq!(req.request.head.version, Version::HTTP_2);
        assert_eq!(req.get_version(), &Version::HTTP_2);
        assert_eq!(req.get_method(), Method::PUT);
        let _ = req.headers_mut();
        let _ = req.send_body("").await;
    }

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

    #[crate::rt_test]
    async fn client_query() {
        let req = Client::new()
            .get("/")
            .query(&[("key1", "val1"), ("key2", "val2")]);
        assert_eq!(req.get_uri().query().unwrap(), "key1=val1&key2=val2");

        let req = Client::new().get("/").query(&InvalidQuery);
        assert!(matches!(req.err, Some(ClientError::Error(_))));
    }
}
