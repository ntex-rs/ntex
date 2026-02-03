use std::marker::PhantomData;

use crate::http::error::HttpError;
use crate::http::header::{HeaderMap, HeaderName, HeaderValue};
use crate::http::{Response, ResponseBuilder, StatusCode};
use crate::util::{Bytes, BytesMut, Either};

use super::error::{
    DefaultError, ErrorContainer, ErrorRenderer, InternalError, WebResponseError,
};
use super::httprequest::HttpRequest;

#[allow(async_fn_in_trait)]
/// Trait implemented by types that can be converted to a http response.
///
/// Types that implement this trait can be used as the return type of a handler.
pub trait Responder<Err = DefaultError> {
    /// Convert itself to http response.
    async fn respond_to(self, req: &HttpRequest) -> Response;

    /// Override a status code for a Responder.
    ///
    /// ```rust
    /// use ntex::http::StatusCode;
    /// use ntex::web::{HttpRequest, Responder};
    ///
    /// fn index(req: HttpRequest) -> impl Responder {
    ///     "Welcome!".with_status(StatusCode::OK)
    /// }
    /// # fn main() {}
    /// ```
    fn with_status(self, status: StatusCode) -> CustomResponder<Self, Err>
    where
        Self: Sized,
    {
        CustomResponder::new(self).with_status(status)
    }

    /// Add header to the Responder's response.
    ///
    /// ```rust
    /// use ntex::web::{self, HttpRequest, Responder};
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct MyObj {
    ///     name: String,
    /// }
    ///
    /// async fn index(req: HttpRequest) -> impl Responder {
    ///     web::types::Json(
    ///         MyObj { name: "Name".to_string() }
    ///     )
    ///     .with_header("x-version", "1.2.3")
    /// }
    /// # fn main() {}
    /// ```
    fn with_header<K, V>(self, key: K, value: V) -> CustomResponder<Self, Err>
    where
        Self: Sized,
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        CustomResponder::new(self).with_header(key, value)
    }
}

impl<Err: ErrorRenderer> Responder<Err> for Response {
    #[inline]
    async fn respond_to(self, _: &HttpRequest) -> Response {
        self
    }
}

impl<Err: ErrorRenderer> Responder<Err> for ResponseBuilder {
    #[inline]
    async fn respond_to(mut self, _: &HttpRequest) -> Response {
        self.finish()
    }
}

impl<T, Err> Responder<Err> for Option<T>
where
    T: Responder<Err>,
    Err: ErrorRenderer,
{
    async fn respond_to(self, req: &HttpRequest) -> Response {
        match self {
            Some(t) => t.respond_to(req).await,
            None => Response::build(StatusCode::NOT_FOUND).finish(),
        }
    }
}

impl<T, E, Err> Responder<Err> for Result<T, E>
where
    T: Responder<Err>,
    E: Into<Err::Container>,
    Err: ErrorRenderer,
{
    async fn respond_to(self, req: &HttpRequest) -> Response {
        match self {
            Ok(val) => val.respond_to(req).await,
            Err(e) => e.into().error_response(req),
        }
    }
}

impl<T, Err> Responder<Err> for (T, StatusCode)
where
    T: Responder<Err>,
    Err: ErrorRenderer,
{
    async fn respond_to(self, req: &HttpRequest) -> Response {
        let mut res = self.0.respond_to(req).await;
        *res.status_mut() = self.1;
        res
    }
}

impl<Err: ErrorRenderer> Responder<Err> for &'static str {
    async fn respond_to(self, _: &HttpRequest) -> Response {
        Response::build(StatusCode::OK)
            .content_type("text/plain; charset=utf-8")
            .body(self)
    }
}

impl<Err: ErrorRenderer> Responder<Err> for &'static [u8] {
    async fn respond_to(self, _: &HttpRequest) -> Response {
        Response::build(StatusCode::OK)
            .content_type("application/octet-stream")
            .body(self)
    }
}

impl<Err: ErrorRenderer> Responder<Err> for String {
    async fn respond_to(self, _: &HttpRequest) -> Response {
        Response::build(StatusCode::OK)
            .content_type("text/plain; charset=utf-8")
            .body(self)
    }
}

impl<Err: ErrorRenderer> Responder<Err> for &String {
    async fn respond_to(self, _: &HttpRequest) -> Response {
        Response::build(StatusCode::OK)
            .content_type("text/plain; charset=utf-8")
            .body(self)
    }
}

impl<Err: ErrorRenderer> Responder<Err> for Bytes {
    async fn respond_to(self, _: &HttpRequest) -> Response {
        Response::build(StatusCode::OK)
            .content_type("application/octet-stream")
            .body(self)
    }
}

impl<Err: ErrorRenderer> Responder<Err> for BytesMut {
    async fn respond_to(self, _: &HttpRequest) -> Response {
        Response::build(StatusCode::OK)
            .content_type("application/octet-stream")
            .body(self)
    }
}

/// Allows to override status code and headers for a responder.
pub struct CustomResponder<T: Responder<Err>, Err> {
    responder: T,
    status: Option<StatusCode>,
    headers: Option<HeaderMap>,
    error: Option<HttpError>,
    _t: PhantomData<Err>,
}

impl<T: Responder<Err>, Err> CustomResponder<T, Err> {
    fn new(responder: T) -> Self {
        CustomResponder {
            responder,
            status: None,
            headers: None,
            error: None,
            _t: PhantomData,
        }
    }

    /// Override a status code for the Responder's response.
    ///
    /// ```rust
    /// use ntex::http::StatusCode;
    /// use ntex::web::{HttpRequest, Responder};
    ///
    /// fn index(req: HttpRequest) -> impl Responder {
    ///     "Welcome!".with_status(StatusCode::OK)
    /// }
    /// # fn main() {}
    /// ```
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = Some(status);
        self
    }

    /// Add header to the Responder's response.
    ///
    /// ```rust
    /// use ntex::web::{self, HttpRequest, Responder};
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct MyObj {
    ///     name: String,
    /// }
    ///
    /// fn index(req: HttpRequest) -> impl Responder {
    ///     web::types::Json(
    ///         MyObj{name: "Name".to_string()}
    ///     )
    ///     .with_header("x-version", "1.2.3")
    /// }
    /// # fn main() {}
    /// ```
    pub fn with_header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if self.headers.is_none() {
            self.headers = Some(HeaderMap::new());
        }

        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => {
                    self.headers.as_mut().unwrap().append(key, value);
                }
                Err(e) => self.error = Some(e.into()),
            },
            Err(e) => self.error = Some(e.into()),
        }
        self
    }
}

impl<T: Responder<Err>, Err: ErrorRenderer> Responder<Err> for CustomResponder<T, Err> {
    async fn respond_to(self, req: &HttpRequest) -> Response {
        let mut res = self.responder.respond_to(req).await;

        if let Some(status) = self.status {
            *res.status_mut() = status;
        }
        if let Some(ref headers) = self.headers {
            for (k, v) in headers {
                res.headers_mut().insert(k.clone(), v.clone());
            }
        }
        res
    }
}

/// Combines two different responder types into a single type
///
/// ```rust
/// use ntex::{web::HttpResponse, util::Either};
///
/// fn index() -> Either<HttpResponse, &'static str> {
///     if is_a_variant() {
///         // <- choose left variant
///         Either::Left(HttpResponse::BadRequest().body("Bad data"))
///     } else {
///         // <- Right variant
///         Either::Right("Hello!")
///     }
/// }
/// # fn is_a_variant() -> bool { true }
/// # fn main() {}
/// ```
impl<A, B, Err> Responder<Err> for Either<A, B>
where
    A: Responder<Err>,
    B: Responder<Err>,
    Err: ErrorRenderer,
{
    async fn respond_to(self, req: &HttpRequest) -> Response {
        match self {
            Either::Left(a) => a.respond_to(req).await,
            Either::Right(b) => b.respond_to(req).await,
        }
    }
}

impl<T, Err> Responder<Err> for InternalError<T, Err>
where
    T: std::fmt::Debug + std::fmt::Display + 'static,
    Err: ErrorRenderer,
{
    async fn respond_to(self, req: &HttpRequest) -> Response {
        self.error_response(req)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::http::Response as HttpResponse;
    use crate::http::body::{Body, ResponseBody};
    use crate::http::header::CONTENT_TYPE;
    use crate::web;
    use crate::web::test::{TestRequest, init_service};

    fn responder<T: Responder<DefaultError>>(responder: T) -> impl Responder<DefaultError> {
        responder
    }

    #[crate::rt_test]
    async fn test_either_responder() {
        let srv = init_service(web::App::new().service(web::resource("/index.html").to(
            |req: HttpRequest| async move {
                if req.query_string().is_empty() {
                    Either::Left(HttpResponse::BadRequest())
                } else {
                    Either::Right("hello")
                }
            },
        )))
        .await;

        let req = TestRequest::with_uri("/index.html").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let req = TestRequest::with_uri("/index.html?query=test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_option_responder() {
        let srv = init_service(
            web::App::new()
                .service(
                    web::resource("/none").to(|| async { Option::<&'static str>::None }),
                )
                .service(web::resource("/some").to(|| async { Some("some") })),
        )
        .await;

        let req = TestRequest::with_uri("/none").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let req = TestRequest::with_uri("/some").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        if let ResponseBody::Body(Body::Bytes(b)) = resp.response().body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"some"));
        } else {
            panic!()
        }
    }

    #[crate::rt_test]
    async fn test_responder() {
        let req = TestRequest::default().to_http_request();

        let resp: HttpResponse = responder("test").respond_to(&req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.get_body_ref(), b"test");
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain; charset=utf-8")
        );

        let resp: HttpResponse = responder(&b"test"[..]).respond_to(&req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.get_body_ref(), b"test");
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/octet-stream")
        );

        let resp: HttpResponse = responder("test".to_string()).respond_to(&req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.get_body_ref(), b"test");
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain; charset=utf-8")
        );

        let resp: HttpResponse = responder(&"test".to_string()).respond_to(&req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.get_body_ref(), b"test");
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain; charset=utf-8")
        );

        let resp: HttpResponse = responder(Bytes::from_static(b"test"))
            .respond_to(&req)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.get_body_ref(), b"test");
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/octet-stream")
        );

        let resp: HttpResponse = responder(BytesMut::from(b"test".as_ref()))
            .respond_to(&req)
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.get_body_ref(), b"test");
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("application/octet-stream")
        );

        // InternalError
        let resp: HttpResponse =
            responder(InternalError::new("err", StatusCode::BAD_REQUEST))
                .respond_to(&req)
                .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[crate::rt_test]
    async fn test_result_responder() {
        let req = TestRequest::default().to_http_request();

        // Result<I, E>
        let resp: HttpResponse = Responder::<DefaultError>::respond_to(
            Ok::<String, std::convert::Infallible>("test".to_string()),
            &req,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.get_body_ref(), b"test");
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("text/plain; charset=utf-8")
        );

        let res = responder(Err::<String, _>(InternalError::new(
            "err",
            StatusCode::BAD_REQUEST,
        )))
        .respond_to(&req)
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[crate::rt_test]
    async fn test_custom_responder() {
        let req = TestRequest::default().to_http_request();
        let res = responder("test".to_string())
            .with_status(StatusCode::BAD_REQUEST)
            .respond_to(&req)
            .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(res.get_body_ref(), b"test");

        let res = responder("test".to_string())
            .with_header("content-type", "json")
            .respond_to(&req)
            .await;

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.get_body_ref(), b"test");
        assert_eq!(
            res.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("json")
        );
    }

    #[crate::rt_test]
    async fn test_tuple_responder_with_status_code() {
        let req = TestRequest::default().to_http_request();
        let res = Responder::<DefaultError>::respond_to(
            ("test".to_string(), StatusCode::BAD_REQUEST),
            &req,
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(res.get_body_ref(), b"test");

        let req = TestRequest::default().to_http_request();
        let res =
            CustomResponder::<_, DefaultError>::new(("test".to_string(), StatusCode::OK))
                .with_header("content-type", "json")
                .respond_to(&req)
                .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.get_body_ref(), b"test");
        assert_eq!(
            res.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("json")
        );
    }
}
