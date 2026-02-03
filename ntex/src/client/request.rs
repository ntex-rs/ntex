use std::{error::Error, fmt, net, rc::Rc};

use base64::{Engine, engine::general_purpose::STANDARD as base64};
#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};
use serde::Serialize;

use crate::http::body::Body;
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::{ConnectionType, Method, RequestHead, RequestHeadType, Uri, Version};
use crate::{time::Millis, util::Bytes, util::Stream};

use super::error::{FreezeRequestError, InvalidUrl, SendRequestError};
use super::sender::PrepForSendingError;
use super::{ClientInner, ClientResponse, frozen::FrozenClientRequest};

/// An HTTP Client request builder
///
/// This type can be used to construct an instance of `ClientRequest` through a
/// builder-like pattern.
///
/// ```rust
/// use ntex::client::Client;
///
/// #[ntex::main]
/// async fn main() {
///    let response = Client::new()
///         .await
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
    pub(crate) head: Box<RequestHead>,
    err: Option<HttpError>,
    addr: Option<net::SocketAddr>,
    #[cfg(feature = "cookie")]
    cookies: Option<CookieJar>,
    response_decompress: bool,
    timeout: Millis,
    config: Rc<ClientInner>,
}

impl ClientRequest {
    /// Create new client request builder.
    pub(super) fn new<U>(method: Method, uri: U, config: Rc<ClientInner>) -> Self
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        ClientRequest {
            config,
            head: Box::new(RequestHead::default()),
            err: None,
            addr: None,
            #[cfg(feature = "cookie")]
            cookies: None,
            timeout: Millis::ZERO,
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

    #[inline]
    /// Get HTTP method of this request
    pub fn get_method(&self) -> &Method {
        &self.head.method
    }

    /// Set HTTP version of this request.
    ///
    /// By default requests's HTTP version depends on network stream
    #[inline]
    pub fn version(mut self, version: Version) -> Self {
        self.head.version = version;
        self
    }

    #[inline]
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
    /// use ntex::{http, client::Client};
    ///
    /// #[ntex::main]
    /// async fn main() {
    ///     let req = Client::new()
    ///         .await
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

    #[inline]
    /// Set connection type of the message
    pub fn set_connection_type(mut self, ctype: ConnectionType) -> Self {
        self.head.set_connection_type(ctype);
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
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.header(
            header::AUTHORIZATION,
            format!("Basic {}", base64.encode(auth)),
        )
    }

    /// Set HTTP bearer authentication header
    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: fmt::Display,
    {
        self.header(header::AUTHORIZATION, format!("Bearer {token}"))
    }

    #[cfg(feature = "cookie")]
    /// Set a cookie
    ///
    /// ```rust
    /// use coo_kie as cookie;
    /// use ntex::client::Client;
    ///
    /// #[ntex::main]
    /// async fn main() {
    ///     let resp = Client::new().await.get("https://www.rust-lang.org")
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
            self.cookies = Some(jar)
        }
        self
    }

    /// Disable automatic decompress of response's body
    pub fn no_decompress(mut self) -> Self {
        self.response_decompress = false;
        self
    }

    /// Set request timeout in millis. Overrides client wide timeout setting.
    ///
    /// Request timeout is the total time before a response must be received.
    /// Default value is 5 seconds.
    pub fn timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.timeout = timeout.into();
        self
    }

    /// This method calls provided closure with builder reference if
    /// value is `true`.
    pub fn if_true<F>(self, value: bool, f: F) -> Self
    where
        F: FnOnce(ClientRequest) -> ClientRequest,
    {
        if value { f(self) } else { self }
    }

    /// This method calls provided closure with builder reference if
    /// value is `Some`.
    pub fn if_some<T, F>(self, value: Option<T>, f: F) -> Self
    where
        F: FnOnce(T, ClientRequest) -> ClientRequest,
    {
        if let Some(val) = value { f(val, self) } else { self }
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
            parts.path_and_query = format!("{path}?{query}").parse().ok();

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
        let slf = self.prep_for_sending()?;
        let request = FrozenClientRequest {
            head: Rc::new(*slf.head),
            addr: slf.addr,
            response_decompress: slf.response_decompress,
            timeout: slf.timeout,
            config: slf.config,
        };

        Ok(request)
    }

    /// Complete request construction and send body.
    pub async fn send_body<B>(self, body: B) -> Result<ClientResponse, SendRequestError>
    where
        B: Into<Body>,
    {
        let slf = self.prep_for_sending()?;

        RequestHeadType::Owned(slf.head)
            .send_body(
                slf.addr,
                slf.response_decompress,
                slf.timeout,
                slf.config,
                body,
            )
            .await
    }

    /// Set a JSON body and generate `ClientRequest`
    pub async fn send_json<T: Serialize>(
        self,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        let slf = self.prep_for_sending()?;

        RequestHeadType::Owned(slf.head)
            .send_json(
                slf.addr,
                slf.response_decompress,
                slf.timeout,
                slf.config,
                value,
            )
            .await
    }

    /// Set a urlencoded body and generate `ClientRequest`
    ///
    /// `ClientRequestBuilder` can not be used after this call.
    pub async fn send_form<T: Serialize>(
        self,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        let slf = self.prep_for_sending()?;

        RequestHeadType::Owned(slf.head)
            .send_form(
                slf.addr,
                slf.response_decompress,
                slf.timeout,
                slf.config,
                value,
            )
            .await
    }

    /// Set an streaming body and generate `ClientRequest`.
    pub async fn send_stream<S, E>(
        self,
        stream: S,
    ) -> Result<ClientResponse, SendRequestError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        let slf = self.prep_for_sending()?;

        RequestHeadType::Owned(slf.head)
            .send_stream(
                slf.addr,
                slf.response_decompress,
                slf.timeout,
                slf.config,
                stream,
            )
            .await
    }

    /// Set an empty body and generate `ClientRequest`.
    pub async fn send(self) -> Result<ClientResponse, SendRequestError> {
        let slf = self.prep_for_sending()?;

        RequestHeadType::Owned(slf.head)
            .send(slf.addr, slf.response_decompress, slf.timeout, slf.config)
            .await
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
                    let name =
                        percent_encode(c.name().as_bytes(), crate::http::helpers::USERINFO);
                    let value = percent_encode(
                        c.value().as_bytes(),
                        crate::http::helpers::USERINFO,
                    );
                    let _ = write!(cookie, "; {name}={value}");
                }
                self.head.headers.insert(
                    header::COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
            }
        }

        let mut slf = self;

        #[cfg(feature = "compress")]
        if slf.response_decompress {
            slf = slf.set_header_if_none(header::ACCEPT_ENCODING, "gzip, deflate");
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
        for (key, val) in &self.head.headers {
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

    #[crate::rt_test]
    async fn test_debug() {
        let request = Client::new().await.get("/").header("x-test", "111");
        let repr = format!("{request:?}");
        assert!(repr.contains("ClientRequest"));
        assert!(repr.contains("x-test"));
    }

    #[crate::rt_test]
    #[allow(clippy::let_underscore_future)]
    async fn test_basics() {
        let mut req = Client::new()
            .await
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
        let req = Client::builder()
            .header(header::CONTENT_TYPE, "111")
            .build(SharedCfg::default())
            .await
            .unwrap()
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
        let req = Client::builder()
            .header(header::CONTENT_TYPE, "111")
            .build(SharedCfg::default())
            .await
            .unwrap()
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
            .await
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

        let req = Client::new().await.get("/").basic_auth("username", None);
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
        let req = Client::new()
            .await
            .get("/")
            .bearer_auth("someS3cr3tAutht0k3n");
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
            .await
            .get("/")
            .query(&[("key1", "val1"), ("key2", "val2")])
            .unwrap();
        assert_eq!(req.get_uri().query().unwrap(), "key1=val1&key2=val2");
    }
}
