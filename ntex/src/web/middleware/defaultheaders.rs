//! Middleware for setting default response headers
use std::convert::TryFrom;
use std::marker::PhantomData;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::future::{ok, FutureExt, LocalBoxFuture, Ready};

use crate::http::error::HttpError;
use crate::http::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use crate::service::{Service, Transform};
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
pub struct DefaultHeaders<E> {
    inner: Rc<Inner>,
    _t: PhantomData<E>,
}

struct Inner {
    ct: bool,
    headers: HeaderMap,
}

impl<E> Default for DefaultHeaders<E> {
    fn default() -> Self {
        DefaultHeaders {
            inner: Rc::new(Inner {
                ct: false,
                headers: HeaderMap::new(),
            }),
            _t: PhantomData,
        }
    }
}

impl<E> DefaultHeaders<E> {
    /// Construct `DefaultHeaders` middleware.
    pub fn new() -> DefaultHeaders<E> {
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
                Err(_) => panic!("Can not create header value"),
            },
            Err(_) => panic!("Can not create header name"),
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

impl<S, E> Transform<S> for DefaultHeaders<E>
where
    S: Service<Request = WebRequest<E>, Response = WebResponse>,
    S::Future: 'static,
{
    type Request = WebRequest<E>;
    type Response = WebResponse;
    type Error = S::Error;
    type InitError = ();
    type Transform = DefaultHeadersMiddleware<S, E>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(DefaultHeadersMiddleware {
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
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

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

        async move {
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
        }
        .boxed_local()
    }
}

#[cfg(test)]
mod tests {
    use futures::future::ok;

    use super::*;
    use crate::http::header::CONTENT_TYPE;
    use crate::service::IntoService;
    use crate::web::request::WebRequest;
    use crate::web::test::{ok_service, TestRequest};
    use crate::web::{DefaultError, Error, HttpResponse};

    #[ntex_rt::test]
    async fn test_default_headers() {
        let mw = DefaultHeaders::<DefaultError>::new()
            .header(CONTENT_TYPE, "0001")
            .new_transform(ok_service())
            .await
            .unwrap();

        let req = TestRequest::default().to_srv_request();
        let resp = mw.call(req).await.unwrap();
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "0001");

        let req = TestRequest::default().to_srv_request();
        let srv =
            |req: WebRequest<DefaultError>| {
                ok::<_, Error>(req.into_response(
                    HttpResponse::Ok().header(CONTENT_TYPE, "0002").finish(),
                ))
            };
        let mw = DefaultHeaders::<DefaultError>::new()
            .header(CONTENT_TYPE, "0001")
            .new_transform(srv.into_service())
            .await
            .unwrap();
        let resp = mw.call(req).await.unwrap();
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "0002");
    }

    #[ntex_rt::test]
    async fn test_content_type() {
        let srv = |req: WebRequest<DefaultError>| {
            ok::<_, Error>(req.into_response(HttpResponse::Ok().finish()))
        };
        let mw = DefaultHeaders::<DefaultError>::new()
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
