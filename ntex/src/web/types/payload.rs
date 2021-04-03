//! Payload/Bytes/String extractors
use std::{future::Future, pin::Pin, str, task::Context, task::Poll};

use bytes::{Bytes, BytesMut};
use encoding_rs::UTF_8;
use futures_core::Stream;
use mime::Mime;

use crate::http::{error, header, HttpMessage};
use crate::util::{next, Either, Ready};
use crate::web::error::{ErrorRenderer, PayloadError};
use crate::web::{FromRequest, HttpRequest};

/// Payload extractor returns request 's payload stream.
///
/// ## Example
///
/// ```rust
/// use bytes::BytesMut;
/// use futures::{Future, Stream};
/// use ntex::web::{self, error, App, HttpResponse};
///
/// /// extract binary data from request
/// async fn index(mut body: web::types::Payload) -> Result<HttpResponse, error::PayloadError>
/// {
///     let mut bytes = BytesMut::new();
///     while let Some(item) = ntex::util::next(&mut body).await {
///         bytes.extend_from_slice(&item?);
///     }
///
///     format!("Body {:?}!", bytes);
///     Ok(HttpResponse::Ok().finish())
/// }
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/index.html").route(
///             web::get().to(index))
///     );
/// }
/// ```
#[derive(Debug)]
pub struct Payload(pub crate::http::Payload);

impl Payload {
    /// Deconstruct to a inner value
    pub fn into_inner(self) -> crate::http::Payload {
        self.0
    }
}

impl Stream for Payload {
    type Item = Result<Bytes, error::PayloadError>;

    #[inline]
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx)
    }
}

/// Get request's payload stream
///
/// ## Example
///
/// ```rust
/// use bytes::BytesMut;
/// use futures::{Future, Stream};
/// use ntex::web::{self, error, App, Error, HttpResponse};
///
/// /// extract binary data from request
/// async fn index(mut body: web::types::Payload) -> Result<HttpResponse, error::PayloadError>
/// {
///     let mut bytes = BytesMut::new();
///     while let Some(item) = ntex::util::next(&mut body).await {
///         bytes.extend_from_slice(&item?);
///     }
///
///     format!("Body {:?}!", bytes);
///     Ok(HttpResponse::Ok().finish())
/// }
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/index.html").route(
///             web::get().to(index))
///     );
/// }
/// ```
impl<Err: ErrorRenderer> FromRequest<Err> for Payload {
    type Error = Err::Container;
    type Future = Ready<Payload, Self::Error>;

    #[inline]
    fn from_request(
        _: &HttpRequest,
        payload: &mut crate::http::Payload,
    ) -> Self::Future {
        Ready::Ok(Payload(payload.take()))
    }
}

/// Request binary data from a request's payload.
///
/// Loads request's payload and construct Bytes instance.
///
/// [**PayloadConfig**](struct.PayloadConfig.html) allows to configure
/// extraction process.
///
/// ## Example
///
/// ```rust
/// use bytes::Bytes;
/// use ntex::web;
///
/// /// extract binary data from request
/// async fn index(body: Bytes) -> String {
///     format!("Body {:?}!", body)
/// }
///
/// fn main() {
///     let app = web::App::new().service(
///         web::resource("/index.html").route(
///             web::get().to(index))
///     );
/// }
/// ```
impl<Err: ErrorRenderer> FromRequest<Err> for Bytes {
    type Error = PayloadError;
    type Future = Either<
        Pin<Box<dyn Future<Output = Result<Bytes, Self::Error>>>>,
        Ready<Bytes, Self::Error>,
    >;

    #[inline]
    fn from_request(
        req: &HttpRequest,
        payload: &mut crate::http::Payload,
    ) -> Self::Future {
        let tmp;
        let cfg = if let Some(cfg) = req.app_data::<PayloadConfig>() {
            cfg
        } else {
            tmp = PayloadConfig::default();
            &tmp
        };

        if let Err(e) = cfg.check_mimetype(req) {
            return Either::Right(Ready::Err(e));
        }

        let limit = cfg.limit;
        let fut = HttpMessageBody::new(req, payload).limit(limit);
        Either::Left(Box::pin(async move { fut.await }))
    }
}

/// Extract text information from a request's body.
///
/// Text extractor automatically decode body according to the request's charset.
///
/// [**PayloadConfig**](struct.PayloadConfig.html) allows to configure
/// extraction process.
///
/// ## Example
///
/// ```rust
/// use ntex::web::{self, App, FromRequest};
///
/// /// extract text data from request
/// async fn index(text: String) -> String {
///     format!("Body {}!", text)
/// }
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/index.html")
///             .app_data(
///                 web::types::PayloadConfig::new(4096)  // <- limit size of the payload
///             )
///             .route(web::get().to(index))  // <- register handler with extractor params
///     );
/// }
/// ```
impl<Err: ErrorRenderer> FromRequest<Err> for String {
    type Error = PayloadError;
    type Future = Either<
        Pin<Box<dyn Future<Output = Result<String, Self::Error>>>>,
        Ready<String, Self::Error>,
    >;

    #[inline]
    fn from_request(
        req: &HttpRequest,
        payload: &mut crate::http::Payload,
    ) -> Self::Future {
        let tmp;
        let cfg = if let Some(cfg) = req.app_data::<PayloadConfig>() {
            cfg
        } else {
            tmp = PayloadConfig::default();
            &tmp
        };

        // check content-type
        if let Err(e) = cfg.check_mimetype(req) {
            return Either::Right(Ready::Err(e));
        }

        // check charset
        let encoding = match req.encoding() {
            Ok(enc) => enc,
            Err(e) => return Either::Right(Ready::Err(PayloadError::from(e))),
        };
        let limit = cfg.limit;
        let fut = HttpMessageBody::new(req, payload).limit(limit);

        Either::Left(Box::pin(async move {
            let body = fut.await?;

            if encoding == UTF_8 {
                Ok(str::from_utf8(body.as_ref())
                    .map_err(|_| PayloadError::Decoding)?
                    .to_owned())
            } else {
                Ok(encoding
                    .decode_without_bom_handling_and_without_replacement(&body)
                    .map(|s| s.into_owned())
                    .ok_or(PayloadError::Decoding)?)
            }
        }))
    }
}
/// Payload configuration for request's payload.
#[derive(Clone, Debug)]
pub struct PayloadConfig {
    limit: usize,
    mimetype: Option<Mime>,
}

impl PayloadConfig {
    /// Create `PayloadConfig` instance and set max size of payload.
    pub fn new(limit: usize) -> Self {
        PayloadConfig {
            limit,
            ..Default::default()
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Set required mime-type of the request. By default mime type is not
    /// enforced.
    pub fn mimetype(mut self, mt: Mime) -> Self {
        self.mimetype = Some(mt);
        self
    }

    fn check_mimetype(&self, req: &HttpRequest) -> Result<(), PayloadError> {
        // check content-type
        if let Some(ref mt) = self.mimetype {
            match req.mime_type() {
                Ok(Some(ref req_mt)) => {
                    if mt != req_mt {
                        return Err(PayloadError::from(
                            error::ContentTypeError::Unexpected,
                        ));
                    }
                }
                Ok(None) => {
                    return Err(PayloadError::from(error::ContentTypeError::Expected));
                }
                Err(err) => {
                    return Err(err.into());
                }
            }
        }
        Ok(())
    }
}

impl Default for PayloadConfig {
    fn default() -> Self {
        PayloadConfig {
            limit: 262_144,
            mimetype: None,
        }
    }
}

/// Future that resolves to a complete http message body.
///
/// Load http message body.
///
/// By default only 256Kb payload reads to a memory, then
/// `PayloadError::Overflow` get returned. Use `MessageBody::limit()`
/// method to change upper limit.
struct HttpMessageBody {
    limit: usize,
    length: Option<usize>,
    #[cfg(feature = "compress")]
    stream: Option<crate::http::encoding::Decoder<crate::http::Payload>>,
    #[cfg(not(feature = "compress"))]
    stream: Option<crate::http::Payload>,
    err: Option<PayloadError>,
    fut: Option<Pin<Box<dyn Future<Output = Result<Bytes, PayloadError>>>>>,
}

impl HttpMessageBody {
    /// Create `MessageBody` for request.
    fn new(req: &HttpRequest, payload: &mut crate::http::Payload) -> HttpMessageBody {
        let mut len = None;
        if let Some(l) = req.headers().get(&header::CONTENT_LENGTH) {
            if let Ok(s) = l.to_str() {
                if let Ok(l) = s.parse::<usize>() {
                    len = Some(l)
                } else {
                    return Self::err(PayloadError::Payload(
                        error::PayloadError::UnknownLength,
                    ));
                }
            } else {
                return Self::err(PayloadError::Payload(
                    error::PayloadError::UnknownLength,
                ));
            }
        }

        #[cfg(feature = "compress")]
        let stream = Some(crate::http::encoding::Decoder::from_headers(
            payload.take(),
            req.headers(),
        ));
        #[cfg(not(feature = "compress"))]
        let stream = Some(payload.take());

        HttpMessageBody {
            stream,
            limit: 262_144,
            length: len,
            fut: None,
            err: None,
        }
    }

    /// Change max size of payload. By default max size is 256Kb
    fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    fn err(e: PayloadError) -> Self {
        HttpMessageBody {
            stream: None,
            limit: 262_144,
            fut: None,
            err: Some(e),
            length: None,
        }
    }
}

impl Future for HttpMessageBody {
    type Output = Result<Bytes, PayloadError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(ref mut fut) = self.fut {
            return Pin::new(fut).poll(cx);
        }

        if let Some(err) = self.err.take() {
            return Poll::Ready(Err(err));
        }

        if let Some(len) = self.length.take() {
            if len > self.limit {
                return Poll::Ready(Err(PayloadError::from(
                    error::PayloadError::Overflow,
                )));
            }
        }

        // future
        let limit = self.limit;
        let mut stream = self.stream.take().unwrap();
        self.fut = Some(Box::pin(async move {
            let mut body = BytesMut::with_capacity(8192);

            while let Some(item) = next(&mut stream).await {
                let chunk = item?;
                if body.len() + chunk.len() > limit {
                    return Err(PayloadError::from(error::PayloadError::Overflow));
                } else {
                    body.extend_from_slice(&chunk);
                }
            }
            Ok(body.freeze())
        }));
        self.poll(cx)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::http::header;
    use crate::web::test::{from_request, TestRequest};

    #[crate::rt_test]
    async fn test_payload_config() {
        let req = TestRequest::default().to_http_request();
        let cfg = PayloadConfig::default().mimetype(mime::APPLICATION_JSON);
        assert!(cfg.check_mimetype(&req).is_err());

        let req = TestRequest::with_header(
            header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .to_http_request();
        assert!(cfg.check_mimetype(&req).is_err());

        let req = TestRequest::with_header(header::CONTENT_TYPE, "application/json")
            .to_http_request();
        assert!(cfg.check_mimetype(&req).is_ok());
    }

    #[crate::rt_test]
    async fn test_payload() {
        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "11")
            .set_payload(Bytes::from_static(b"hello=world"))
            .to_http_parts();

        let mut s = from_request::<Payload>(&req, &mut pl)
            .await
            .unwrap()
            .into_inner();
        let b = next(&mut s).await.unwrap().unwrap();
        assert_eq!(b, Bytes::from_static(b"hello=world"));
    }

    #[crate::rt_test]
    async fn test_bytes() {
        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "11")
            .set_payload(Bytes::from_static(b"hello=world"))
            .to_http_parts();

        let s = from_request::<Bytes>(&req, &mut pl).await.unwrap();
        assert_eq!(s, Bytes::from_static(b"hello=world"));

        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "11")
            .set_payload(Bytes::from_static(b"hello=world"))
            .data(PayloadConfig::default().mimetype(mime::APPLICATION_JSON))
            .to_http_parts();
        assert!(from_request::<Bytes>(&req, &mut pl).await.is_err());
    }

    #[crate::rt_test]
    async fn test_string() {
        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "11")
            .set_payload(Bytes::from_static(b"hello=world"))
            .to_http_parts();

        let s = from_request::<String>(&req, &mut pl).await.unwrap();
        assert_eq!(s, "hello=world");

        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "11")
            .header(header::CONTENT_TYPE, "text/plain; charset=cp1251")
            .set_payload(Bytes::from_static(b"hello=world"))
            .to_http_parts();
        let s = from_request::<String>(&req, &mut pl).await.unwrap();
        assert_eq!(s, "hello=world");

        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "11")
            .set_payload(Bytes::from_static(b"hello=world"))
            .data(PayloadConfig::default().mimetype(mime::APPLICATION_JSON))
            .to_http_parts();
        assert!(from_request::<String>(&req, &mut pl).await.is_err());
    }

    #[crate::rt_test]
    async fn test_message_body() {
        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "xxxx")
            .to_srv_request()
            .into_parts();
        let res = HttpMessageBody::new(&req, &mut pl).await;
        match res.err().unwrap() {
            PayloadError::Payload(error::PayloadError::UnknownLength) => (),
            _ => unreachable!("error"),
        }

        let (req, mut pl) = TestRequest::with_header(header::CONTENT_LENGTH, "1000000")
            .to_srv_request()
            .into_parts();
        let res = HttpMessageBody::new(&req, &mut pl).await;
        match res.err().unwrap() {
            PayloadError::Payload(error::PayloadError::Overflow) => (),
            _ => unreachable!("error"),
        }

        let (req, mut pl) = TestRequest::default()
            .set_payload(Bytes::from_static(b"test"))
            .to_http_parts();
        let res = HttpMessageBody::new(&req, &mut pl).await;
        assert_eq!(res.ok().unwrap(), Bytes::from_static(b"test"));

        let (req, mut pl) = TestRequest::default()
            .set_payload(Bytes::from_static(b"11111111111111"))
            .to_http_parts();
        let res = HttpMessageBody::new(&req, &mut pl).limit(5).await;
        match res.err().unwrap() {
            PayloadError::Payload(error::PayloadError::Overflow) => (),
            _ => unreachable!("error"),
        }
    }
}
