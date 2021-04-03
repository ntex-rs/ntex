//! Middleware for setting default response headers
use std::task::{Context, Poll};
use std::{convert::TryFrom, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use crate::http::error::HttpError;
use crate::http::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use crate::service::{Service, Transform};
use crate::util::Ready;
use crate::web::dev::{WebRequest, WebResponse};

/// `Middleware` for setting default response headers.
///
/// This middleware does not set header if response headers already contains it.
///
/// ```rust
/// use ntex::http;
/// use ntex::web::{self, middleware, App, HttpResponse};
///
/// fn main() {
///     let app = App::new()
///         .wrap(middleware::DefaultHeaders::new().header("X-Version", "0.2"))
///         .service(
///             web::resource("/test")
///                 .route(web::get().to(|| async { HttpResponse::Ok() }))
///                 .route(web::method(http::Method::HEAD).to(|| async { HttpResponse::MethodNotAllowed() }))
///         );
/// }
/// ```
#[derive(Clone)]
pub struct DefaultHeaders {
    inner: Rc<Inner>,
}

struct Inner {
    ct: bool,
    headers: HeaderMap,
}

impl Default for DefaultHeaders {
    fn default() -> Self {
        DefaultHeaders {
            inner: Rc::new(Inner {
                ct: false,
                headers: HeaderMap::new(),
            }),
        }
    }
}

impl DefaultHeaders {
    /// Construct `DefaultHeaders` middleware.
    pub fn new() -> DefaultHeaders {
        DefaultHeaders::default()
    }

    /// Set a header.
    #[inline]
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        #[allow(clippy::match_wild_err_arm)]
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => {
                    Rc::get_mut(&mut self.inner)
                        .expect("Multiple copies exist")
                        .headers
                        .append(key, value);
                }
                Err(_) => panic!("Cannot create header value"),
            },
            Err(_) => panic!("Cannot create header name"),
        }
        self
    }

    /// Set *CONTENT-TYPE* header if response does not contain this header.
    pub fn content_type(mut self) -> Self {
        Rc::get_mut(&mut self.inner)
            .expect("Multiple copies exist")
            .ct = true;
        self
    }
}

impl<S, E> Transform<S> for DefaultHeaders
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
    S::Future: 'static,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type InitError = ();
    type Transform = DefaultHeadersMiddleware<S, E>;
    type Future = Ready<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        Ready::Ok(DefaultHeadersMiddleware {
            service,
            inner: self.inner.clone(),
            _t: PhantomData,
        })
    }
}

pub struct DefaultHeadersMiddleware<S, E> {
    service: S,
    inner: Rc<Inner>,
    _t: PhantomData<E>,
}

impl<S, E> Service for DefaultHeadersMiddleware<S, E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
    S::Future: 'static,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, req: WebRequest<E>) -> Self::Future {
        let inner = self.inner.clone();
        let fut = self.service.call(req);

        Box::pin(async move {
            let mut res = fut.await?;

            // set response headers
            for (key, value) in inner.headers.iter() {
                if !res.headers().contains_key(key) {
                    res.headers_mut().insert(key.clone(), value.clone());
                }
            }
            // default content-type
            if inner.ct && !res.headers().contains_key(&CONTENT_TYPE) {
                res.headers_mut().insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
            }
            Ok(res)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::header::CONTENT_TYPE;
    use crate::service::IntoService;
    use crate::util::lazy;
    use crate::web::request::WebRequest;
    use crate::web::test::{ok_service, TestRequest};
    use crate::web::{DefaultError, Error, HttpResponse};

    #[crate::rt_test]
    async fn test_default_headers() {
        let mw = DefaultHeaders::new()
            .header(CONTENT_TYPE, "0001")
            .new_transform(ok_service())
            .await
            .unwrap();

        assert!(lazy(|cx| mw.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| mw.poll_shutdown(cx, true).is_ready()).await);

        let req = TestRequest::default().to_srv_request();
        let resp = mw.call(req).await.unwrap();
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "0001");

        let req = TestRequest::default().to_srv_request();
        let srv =
            |req: WebRequest<DefaultError>| async move {
                Ok::<_, Error>(req.into_response(
                    HttpResponse::Ok().header(CONTENT_TYPE, "0002").finish(),
                ))
            };
        let mw = DefaultHeaders::new()
            .header(CONTENT_TYPE, "0001")
            .new_transform(srv.into_service())
            .await
            .unwrap();
        let resp = mw.call(req).await.unwrap();
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "0002");
    }

    #[crate::rt_test]
    async fn test_content_type() {
        let srv = |req: WebRequest<DefaultError>| async move {
            Ok::<_, Error>(req.into_response(HttpResponse::Ok().finish()))
        };
        let mw = DefaultHeaders::new()
            .content_type()
            .new_transform(srv.into_service())
            .await
            .unwrap();

        let req = TestRequest::default().to_srv_request();
        let resp = mw.call(req).await.unwrap();
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }
}
