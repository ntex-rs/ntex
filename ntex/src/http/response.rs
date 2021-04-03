//! Http response
use std::{cell::Ref, cell::RefMut, convert::TryFrom, error::Error, fmt, str};

use bytes::{Bytes, BytesMut};
use serde::Serialize;

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use crate::http::body::{Body, BodyStream, MessageBody, ResponseBody};
use crate::http::error::{HttpError, ResponseError};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::message::{ConnectionType, Message, ResponseHead};
use crate::http::StatusCode;
use crate::{util::Extensions, Stream};

/// An HTTP Response
pub struct Response<B = Body> {
    body: ResponseBody<B>,
    head: Message<ResponseHead>,
}

impl Response<Body> {
    /// Create http response builder with specific status.
    #[inline]
    pub fn build(status: StatusCode) -> ResponseBuilder {
        ResponseBuilder::new(status)
    }

    /// Create http response builder
    #[inline]
    pub fn build_from<T: Into<ResponseBuilder>>(source: T) -> ResponseBuilder {
        source.into()
    }

    /// Constructs a response
    #[inline]
    pub fn new(status: StatusCode) -> Response {
        Response {
            head: Message::with_status(status),
            body: ResponseBody::Body(Body::Empty),
        }
    }

    /// Convert response to response with body
    pub fn into_body<B>(self) -> Response<B> {
        let b = match self.body {
            ResponseBody::Body(b) => b,
            ResponseBody::Other(b) => b,
        };
        Response {
            head: self.head,
            body: ResponseBody::Other(b),
        }
    }
}

impl<B> Response<B> {
    /// Constructs a response with body
    #[inline]
    pub fn with_body(status: StatusCode, body: B) -> Response<B> {
        Response {
            head: Message::with_status(status),
            body: ResponseBody::Body(body),
        }
    }

    #[inline]
    /// Http message part of the response
    pub fn head(&self) -> &ResponseHead {
        &*self.head
    }

    #[inline]
    /// Mutable reference to a http message part of the response
    pub fn head_mut(&mut self) -> &mut ResponseHead {
        &mut *self.head
    }

    /// Get the response status code
    #[inline]
    pub fn status(&self) -> StatusCode {
        self.head.status
    }

    /// Set the `StatusCode` for this response
    #[inline]
    pub fn status_mut(&mut self) -> &mut StatusCode {
        &mut self.head.status
    }

    /// Get the headers from the response
    #[inline]
    pub fn headers(&self) -> &HeaderMap {
        &self.head.headers
    }

    /// Get a mutable reference to the headers
    #[inline]
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.head.headers
    }

    #[cfg(feature = "cookie")]
    /// Get an iterator for the cookies set by this response
    #[inline]
    pub fn cookies(&self) -> CookieIter<'_> {
        CookieIter {
            iter: self.head.headers.get_all(header::SET_COOKIE),
        }
    }

    #[cfg(feature = "cookie")]
    /// Add a cookie to this response
    #[inline]
    pub fn add_cookie(&mut self, cookie: &Cookie<'_>) -> Result<(), HttpError> {
        let h = &mut self.head.headers;
        HeaderValue::from_str(&cookie.to_string())
            .map(|c| {
                h.append(header::SET_COOKIE, c);
            })
            .map_err(|e| e.into())
    }

    #[cfg(feature = "cookie")]
    /// Remove all cookies with the given name from this response. Returns
    /// the number of cookies removed.
    #[inline]
    pub fn del_cookie(&mut self, name: &str) -> usize {
        let h = &mut self.head.headers;
        let vals: Vec<HeaderValue> = h
            .get_all(header::SET_COOKIE)
            .map(|v| v.to_owned())
            .collect();
        h.remove(header::SET_COOKIE);

        let mut count: usize = 0;
        for v in vals {
            if let Ok(s) = v.to_str() {
                if let Ok(c) = Cookie::parse_encoded(s) {
                    if c.name() == name {
                        count += 1;
                        continue;
                    }
                }
            }
            h.append(header::SET_COOKIE, v);
        }
        count
    }

    /// Connection upgrade status
    #[inline]
    pub fn upgrade(&self) -> bool {
        self.head.upgrade()
    }

    /// Keep-alive status for this connection
    pub fn keep_alive(&self) -> bool {
        self.head.keep_alive()
    }

    /// Responses extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        self.head.extensions.borrow()
    }

    /// Mutable reference to a the response's extensions
    #[inline]
    pub fn extensions_mut(&mut self) -> RefMut<'_, Extensions> {
        self.head.extensions.borrow_mut()
    }

    /// Get body of this response
    #[inline]
    pub fn body(&self) -> &ResponseBody<B> {
        &self.body
    }

    /// Set a body
    pub fn set_body<B2>(self, body: B2) -> Response<B2> {
        Response {
            head: self.head,
            body: ResponseBody::Body(body),
        }
    }

    /// Split response and body
    pub fn into_parts(self) -> (Response<()>, ResponseBody<B>) {
        (
            Response {
                head: self.head,
                body: ResponseBody::Body(()),
            },
            self.body,
        )
    }

    /// Drop request's body
    pub fn drop_body(self) -> Response<()> {
        Response {
            head: self.head,
            body: ResponseBody::Body(()),
        }
    }

    /// Set a body and return previous body value
    pub(crate) fn replace_body<B2>(self, body: B2) -> (Response<B2>, ResponseBody<B>) {
        (
            Response {
                head: self.head,
                body: ResponseBody::Body(body),
            },
            self.body,
        )
    }

    /// Set a body and return previous body value
    pub fn map_body<F, B2>(mut self, f: F) -> Response<B2>
    where
        F: FnOnce(&mut ResponseHead, ResponseBody<B>) -> ResponseBody<B2>,
    {
        let body = f(&mut self.head, self.body);

        Response {
            body,
            head: self.head,
        }
    }

    /// Extract response body
    pub fn take_body(&mut self) -> ResponseBody<B> {
        self.body.take_body()
    }
}

impl<B: MessageBody> fmt::Debug for Response<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let res = writeln!(
            f,
            "\nResponse {:?} {}{}",
            self.head.version,
            self.head.status,
            self.head.reason.unwrap_or(""),
        );
        let _ = writeln!(f, "  headers:");
        for (key, val) in self.head.headers.iter() {
            let _ = writeln!(f, "    {:?}: {:?}", key, val);
        }
        let _ = writeln!(f, "  body: {:?}", self.body.size());
        res
    }
}

#[cfg(feature = "cookie")]
pub struct CookieIter<'a> {
    iter: header::GetAll<'a>,
}

#[cfg(feature = "cookie")]
impl<'a> Iterator for CookieIter<'a> {
    type Item = Cookie<'a>;

    #[inline]
    fn next(&mut self) -> Option<Cookie<'a>> {
        for v in self.iter.by_ref() {
            if let Ok(c) = Cookie::parse_encoded(v.to_str().ok()?) {
                return Some(c);
            }
        }
        None
    }
}

/// An HTTP response builder
///
/// This type can be used to construct an instance of `Response` through a
/// builder-like pattern.
pub struct ResponseBuilder {
    head: Option<Message<ResponseHead>>,
    err: Option<HttpError>,
    #[cfg(feature = "cookie")]
    cookies: Option<CookieJar>,
}

impl ResponseBuilder {
    #[inline]
    /// Create response builder
    pub fn new(status: StatusCode) -> Self {
        ResponseBuilder {
            head: Some(Message::with_status(status)),
            err: None,
            #[cfg(feature = "cookie")]
            cookies: None,
        }
    }

    /// Set HTTP status code of this response.
    #[inline]
    pub fn status(&mut self, status: StatusCode) -> &mut Self {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            parts.status = status;
        }
        self
    }

    /// Append a header to existing headers.
    ///
    /// ```rust
    /// use ntex::http::{header, Request, Response};
    ///
    /// fn index(req: Request) -> Response {
    ///     Response::Ok()
    ///         .header("X-TEST", "value")
    ///         .header(header::CONTENT_TYPE, "application/json")
    ///         .finish()
    /// }
    /// ```
    pub fn header<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            match HeaderName::try_from(key) {
                Ok(key) => match HeaderValue::try_from(value) {
                    Ok(value) => {
                        parts.headers.append(key, value);
                    }
                    Err(e) => self.err = Some(log_error(e)),
                },
                Err(e) => self.err = Some(log_error(e)),
            };
        }
        self
    }

    /// Set a header.
    ///
    /// ```rust
    /// use ntex::http::{header, Request, Response};
    ///
    /// fn index(req: Request) -> Response {
    ///     Response::Ok()
    ///         .set_header("X-TEST", "value")
    ///         .set_header(header::CONTENT_TYPE, "application/json")
    ///         .finish()
    /// }
    /// ```
    pub fn set_header<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            match HeaderName::try_from(key) {
                Ok(key) => match HeaderValue::try_from(value) {
                    Ok(value) => {
                        parts.headers.insert(key, value);
                    }
                    Err(e) => self.err = Some(log_error(e)),
                },
                Err(e) => self.err = Some(log_error(e)),
            };
        }
        self
    }

    /// Set the custom reason for the response.
    #[inline]
    pub fn reason(&mut self, reason: &'static str) -> &mut Self {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            parts.reason = Some(reason);
        }
        self
    }

    /// Set connection type to KeepAlive
    #[inline]
    pub fn keep_alive(&mut self) -> &mut Self {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            parts.set_connection_type(ConnectionType::KeepAlive);
        }
        self
    }

    /// Set connection type to Upgrade
    #[inline]
    pub fn upgrade<V>(&mut self, value: V) -> &mut Self
    where
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            parts.set_connection_type(ConnectionType::Upgrade);
        }
        self.set_header(header::UPGRADE, value)
    }

    /// Force close connection, even if it is marked as keep-alive
    #[inline]
    pub fn force_close(&mut self) -> &mut Self {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            parts.set_connection_type(ConnectionType::Close);
        }
        self
    }

    /// Disable chunked transfer encoding for HTTP/1.1 streaming responses.
    #[inline]
    pub fn no_chunking(&mut self) -> &mut Self {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            parts.no_chunking(true);
        }
        self
    }

    /// Set response content type
    #[inline]
    pub fn content_type<V>(&mut self, value: V) -> &mut Self
    where
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if let Some(parts) = parts(&mut self.head, &self.err) {
            match HeaderValue::try_from(value) {
                Ok(value) => {
                    parts.headers.insert(header::CONTENT_TYPE, value);
                }
                Err(e) => self.err = Some(log_error(e)),
            };
        }
        self
    }

    /// Set content length
    #[inline]
    pub fn content_length(&mut self, len: u64) -> &mut Self {
        self.header(header::CONTENT_LENGTH, len)
    }

    #[cfg(feature = "cookie")]
    /// Set a cookie
    ///
    /// ```rust
    /// use coo_kie as cookie;
    /// use ntex::http::{Request, Response};
    ///
    /// fn index(req: Request) -> Response {
    ///     Response::Ok()
    ///         .cookie(
    ///             cookie::Cookie::build("name", "value")
    ///                 .domain("www.rust-lang.org")
    ///                 .path("/")
    ///                 .secure(true)
    ///                 .http_only(true)
    ///                 .finish(),
    ///         )
    ///         .finish()
    /// }
    /// ```
    pub fn cookie<'c>(&mut self, cookie: Cookie<'c>) -> &mut Self {
        if self.cookies.is_none() {
            let mut jar = CookieJar::new();
            jar.add(cookie.into_owned());
            self.cookies = Some(jar)
        } else {
            self.cookies.as_mut().unwrap().add(cookie.into_owned());
        }
        self
    }

    #[cfg(feature = "cookie")]
    /// Remove cookie
    ///
    /// ```rust
    /// use ntex::http::{Request, Response, HttpMessage};
    ///
    /// fn index(req: Request) -> Response {
    ///     let mut builder = Response::Ok();
    ///
    ///     if let Some(ref cookie) = req.cookie("name") {
    ///         builder.del_cookie(cookie);
    ///     }
    ///
    ///     builder.finish()
    /// }
    /// ```
    pub fn del_cookie<'a>(&mut self, cookie: &Cookie<'a>) -> &mut Self {
        if self.cookies.is_none() {
            self.cookies = Some(CookieJar::new())
        }
        let jar = self.cookies.as_mut().unwrap();
        let cookie = cookie.clone().into_owned();
        jar.add_original(cookie.clone());
        jar.remove(cookie);
        self
    }

    /// This method calls provided closure with builder reference if value is
    /// true.
    pub fn if_true<F>(&mut self, value: bool, f: F) -> &mut Self
    where
        F: FnOnce(&mut ResponseBuilder),
    {
        if value {
            f(self);
        }
        self
    }

    /// This method calls provided closure with builder reference if value is
    /// Some.
    pub fn if_some<T, F>(&mut self, value: Option<T>, f: F) -> &mut Self
    where
        F: FnOnce(T, &mut ResponseBuilder),
    {
        if let Some(val) = value {
            f(val, self);
        }
        self
    }

    /// Responses extensions
    #[inline]
    pub fn extensions(&self) -> Ref<'_, Extensions> {
        let head = self.head.as_ref().expect("cannot reuse response builder");
        head.extensions.borrow()
    }

    /// Mutable reference to a the response's extensions
    #[inline]
    pub fn extensions_mut(&mut self) -> RefMut<'_, Extensions> {
        let head = self.head.as_ref().expect("cannot reuse response builder");
        head.extensions.borrow_mut()
    }

    #[inline]
    /// Set a body and generate `Response`.
    ///
    /// `ResponseBuilder` can not be used after this call.
    pub fn body<B: Into<Body>>(&mut self, body: B) -> Response {
        self.message_body(body.into())
    }

    /// Set a body and generate `Response`.
    ///
    /// `ResponseBuilder` can not be used after this call.
    pub fn message_body<B>(&mut self, body: B) -> Response<B> {
        if let Some(e) = self.err.take() {
            return Response::from(e).into_body();
        }

        #[allow(unused_mut)]
        let mut response = self.head.take().expect("cannot reuse response builder");

        #[cfg(feature = "cookie")]
        {
            if let Some(ref jar) = self.cookies {
                for cookie in jar.delta() {
                    match HeaderValue::from_str(&cookie.to_string()) {
                        Ok(val) => response.headers.append(header::SET_COOKIE, val),
                        Err(e) => return Response::from(HttpError::from(e)).into_body(),
                    };
                }
            }
        }

        Response {
            head: response,
            body: ResponseBody::Body(body),
        }
    }

    #[inline]
    /// Set a streaming body and generate `Response`.
    ///
    /// `ResponseBuilder` can not be used after this call.
    pub fn streaming<S, E>(&mut self, stream: S) -> Response
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        self.body(Body::from_message(BodyStream::new(stream)))
    }

    /// Set a json body and generate `Response`
    ///
    /// `ResponseBuilder` can not be used after this call.
    pub fn json<T: Serialize>(&mut self, value: &T) -> Response {
        match serde_json::to_string(value) {
            Ok(body) => {
                let contains = if let Some(parts) = parts(&mut self.head, &self.err) {
                    parts.headers.contains_key(header::CONTENT_TYPE)
                } else {
                    true
                };
                if !contains {
                    self.header(header::CONTENT_TYPE, "application/json");
                }

                self.body(Body::from(body))
            }
            Err(e) => (&e).into(),
        }
    }

    #[inline]
    /// Set an empty body and generate `Response`
    ///
    /// `ResponseBuilder` can not be used after this call.
    pub fn finish(&mut self) -> Response {
        self.body(Body::Empty)
    }

    /// This method construct new `ResponseBuilder`
    pub fn take(&mut self) -> ResponseBuilder {
        ResponseBuilder {
            head: self.head.take(),
            err: self.err.take(),
            #[cfg(feature = "cookie")]
            cookies: self.cookies.take(),
        }
    }
}

#[inline]
fn parts<'a>(
    parts: &'a mut Option<Message<ResponseHead>>,
    err: &Option<HttpError>,
) -> Option<&'a mut ResponseHead> {
    if err.is_some() {
        return None;
    }
    parts.as_mut().map(|r| &mut **r)
}

/// Convert `Response` to a `ResponseBuilder`. Body get dropped.
impl<B> From<Response<B>> for ResponseBuilder {
    fn from(res: Response<B>) -> ResponseBuilder {
        #[cfg(feature = "cookie")]
        {
            // If this response has cookies, load them into a jar
            let mut jar: Option<CookieJar> = None;
            for c in res.cookies() {
                if let Some(ref mut j) = jar {
                    j.add_original(c.into_owned());
                } else {
                    let mut j = CookieJar::new();
                    j.add_original(c.into_owned());
                    jar = Some(j);
                }
            }

            ResponseBuilder {
                head: Some(res.head),
                err: None,
                cookies: jar,
            }
        }
        #[cfg(not(feature = "cookie"))]
        {
            ResponseBuilder {
                head: Some(res.head),
                err: None,
            }
        }
    }
}

/// Convert `ResponseHead` to a `ResponseBuilder`
impl<'a> From<&'a ResponseHead> for ResponseBuilder {
    fn from(head: &'a ResponseHead) -> ResponseBuilder {
        let mut msg = Message::with_status(head.status);
        msg.version = head.version;
        msg.reason = head.reason;
        for (k, v) in &head.headers {
            msg.headers.append(k.clone(), v.clone());
        }
        msg.no_chunking(!head.chunked());

        #[cfg(feature = "cookie")]
        {
            // If this response has cookies, load them into a jar
            let mut jar: Option<CookieJar> = None;

            let cookies = CookieIter {
                iter: head.headers.get_all(header::SET_COOKIE),
            };
            for c in cookies {
                if let Some(ref mut j) = jar {
                    j.add_original(c.into_owned());
                } else {
                    let mut j = CookieJar::new();
                    j.add_original(c.into_owned());
                    jar = Some(j);
                }
            }
            ResponseBuilder {
                head: Some(msg),
                err: None,
                cookies: jar,
            }
        }

        #[cfg(not(feature = "cookie"))]
        {
            ResponseBuilder {
                head: Some(msg),
                err: None,
            }
        }
    }
}

impl fmt::Debug for ResponseBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head = self.head.as_ref().unwrap();

        let res = writeln!(
            f,
            "\nResponseBuilder {:?} {}{}",
            head.version,
            head.status,
            head.reason.unwrap_or(""),
        );
        let _ = writeln!(f, "  headers:");
        for (key, val) in head.headers.iter() {
            let _ = writeln!(f, "    {:?}: {:?}", key, val);
        }
        res
    }
}

fn log_error<T: Into<HttpError>>(err: T) -> HttpError {
    let e = err.into();
    error!("Error in ResponseBuilder {}", e);
    e
}

/// Helper converters
impl<I: Into<Response>, E> From<Result<I, E>> for Response
where
    E: ResponseError + fmt::Debug,
{
    fn from(res: Result<I, E>) -> Self {
        match res {
            Ok(val) => val.into(),
            Err(err) => (&err).into(),
        }
    }
}

impl From<ResponseBuilder> for Response {
    fn from(mut builder: ResponseBuilder) -> Self {
        builder.finish()
    }
}

impl From<&'static str> for Response {
    fn from(val: &'static str) -> Self {
        Response::Ok()
            .content_type("text/plain; charset=utf-8")
            .body(val)
    }
}

impl From<&'static [u8]> for Response {
    fn from(val: &'static [u8]) -> Self {
        Response::Ok()
            .content_type("application/octet-stream")
            .body(val)
    }
}

impl From<String> for Response {
    fn from(val: String) -> Self {
        Response::Ok()
            .content_type("text/plain; charset=utf-8")
            .body(val)
    }
}

impl<'a> From<&'a String> for Response {
    fn from(val: &'a String) -> Self {
        Response::Ok()
            .content_type("text/plain; charset=utf-8")
            .body(val)
    }
}

impl From<Bytes> for Response {
    fn from(val: Bytes) -> Self {
        Response::Ok()
            .content_type("application/octet-stream")
            .body(val)
    }
}

impl From<BytesMut> for Response {
    fn from(val: BytesMut) -> Self {
        Response::Ok()
            .content_type("application/octet-stream")
            .body(val)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::body::Body;
    use crate::http::header::{HeaderValue, CONTENT_TYPE, COOKIE};

    #[test]
    fn test_debug() {
        let resp = Response::Ok()
            .header(COOKIE, HeaderValue::from_static("cookie1=value1; "))
            .header(COOKIE, HeaderValue::from_static("cookie2=value2; "))
            .finish();
        let dbg = format!("{:?}", resp);
        assert!(dbg.contains("Response"));

        let mut resp = Response::Ok();
        resp.header(COOKIE, HeaderValue::from_static("cookie1=value1; "));
        resp.header(COOKIE, HeaderValue::from_static("cookie2=value2; "));
        let dbg = format!("{:?}", resp);
        assert!(dbg.contains("ResponseBuilder"));
    }

    #[cfg(feature = "cookie")]
    #[test]
    fn test_response_cookies() {
        use crate::http::header::{COOKIE, SET_COOKIE};
        use crate::http::httpmessage::HttpMessage;

        let req = crate::http::test::TestRequest::default()
            .header(COOKIE, "cookie1=value1")
            .header(COOKIE, "cookie2=value2")
            .finish();
        let cookies = req.cookies().unwrap();

        let resp = Response::Ok()
            .cookie(
                coo_kie::Cookie::build("name", "value")
                    .domain("www.rust-lang.org")
                    .path("/test")
                    .http_only(true)
                    .max_age(time::Duration::days(1))
                    .finish(),
            )
            .del_cookie(&cookies[1])
            .finish();

        let mut val: Vec<_> = resp
            .headers()
            .get_all(SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        val.sort();
        assert!(val[0].starts_with("cookie1=; Max-Age=0;"));
        assert_eq!(
            val[1],
            "name=value; HttpOnly; Path=/test; Domain=www.rust-lang.org; Max-Age=86400"
        );
    }

    #[cfg(feature = "cookie")]
    #[test]
    fn test_update_response_cookies() {
        let mut r = Response::Ok()
            .cookie(coo_kie::Cookie::new("original", "val100"))
            .finish();

        r.add_cookie(&coo_kie::Cookie::new("cookie2", "val200"))
            .unwrap();
        r.add_cookie(&coo_kie::Cookie::new("cookie2", "val250"))
            .unwrap();
        r.add_cookie(&coo_kie::Cookie::new("cookie3", "val300"))
            .unwrap();

        assert_eq!(r.cookies().count(), 4);
        r.del_cookie("cookie2");

        let mut iter = r.cookies();
        let v = iter.next().unwrap();
        assert_eq!((v.name(), v.value()), ("cookie3", "val300"));
        let v = iter.next().unwrap();
        assert_eq!((v.name(), v.value()), ("original", "val100"));
    }

    #[test]
    fn test_basic_builder() {
        let resp = Response::Ok().header("X-TEST", "value").finish();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn test_upgrade() {
        let resp = Response::build(StatusCode::OK)
            .upgrade("websocket")
            .finish();
        assert!(resp.upgrade());
        assert_eq!(
            resp.headers().get(header::UPGRADE).unwrap(),
            HeaderValue::from_static("websocket")
        );
    }

    #[test]
    fn test_force_close() {
        let resp = Response::build(StatusCode::OK).force_close().finish();
        assert!(!resp.keep_alive())
    }

    #[test]
    fn test_content_type() {
        let resp = Response::build(StatusCode::OK)
            .content_type("text/plain")
            .body(Body::Empty);
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "text/plain")
    }

    #[test]
    fn test_json() {
        let resp = Response::build(StatusCode::OK).json(&vec!["v1", "v2", "v3"]);
        let ct = resp.headers().get(CONTENT_TYPE).unwrap();
        assert_eq!(ct, HeaderValue::from_static("application/json"));
        assert_eq!(resp.body().get_ref(), b"[\"v1\",\"v2\",\"v3\"]");
    }

    #[test]
    fn test_json_ct() {
        let resp = Response::build(StatusCode::OK)
            .header(CONTENT_TYPE, "text/json")
            .json(&vec!["v1", "v2", "v3"]);
        let ct = resp.headers().get(CONTENT_TYPE).unwrap();
        assert_eq!(ct, HeaderValue::from_static("text/json"));
        assert_eq!(resp.body().get_ref(), b"[\"v1\",\"v2\",\"v3\"]");
    }

    #[test]
    fn test_serde_json_in_body() {
        use serde_json::json;
        let resp =
            Response::build(StatusCode::OK).body(json!({"test-key":"test-value"}));
        assert_eq!(resp.body().get_ref(), br#"{"test-key":"test-value"}"#);
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_into_response() {
        let resp: Response = "test".into();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain; charset=utf-8")
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body().get_ref(), b"test");

        let resp: Response = b"test".as_ref().into();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/octet-stream")
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body().get_ref(), b"test");

        let resp: Response = "test".to_owned().into();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain; charset=utf-8")
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body().get_ref(), b"test");

        let resp: Response = (&"test".to_owned()).into();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain; charset=utf-8")
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body().get_ref(), b"test");

        let b = Bytes::from_static(b"test");
        let resp: Response = b.into();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/octet-stream")
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body().get_ref(), b"test");

        let b = Bytes::from_static(b"test");
        let resp: Response = b.into();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/octet-stream")
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body().get_ref(), b"test");

        let b = BytesMut::from("test");
        let resp: Response = b.into();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/octet-stream")
        );

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.body().get_ref(), b"test");
    }

    #[test]
    fn test_into_builder() {
        #[allow(unused_mut)]
        let mut resp: Response = "test".into();
        assert_eq!(resp.status(), StatusCode::OK);

        #[cfg(feature = "cookie")]
        resp.add_cookie(&coo_kie::Cookie::new("cookie1", "val100"))
            .unwrap();
        let (resp, _) = resp.into_parts();

        let mut builder: ResponseBuilder = resp.head().into();
        let resp = builder.status(StatusCode::BAD_REQUEST).finish();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        #[cfg(feature = "cookie")]
        {
            let cookie = resp.cookies().next().unwrap();
            assert_eq!((cookie.name(), cookie.value()), ("cookie1", "val100"));
        }

        let mut builder: ResponseBuilder = resp.into();
        let resp = builder.status(StatusCode::BAD_REQUEST).finish();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        #[cfg(feature = "cookie")]
        {
            let cookie = resp.cookies().next().unwrap();
            assert_eq!((cookie.name(), cookie.value()), ("cookie1", "val100"));
        }
    }
}
