//! Middleware for setting default response headers
use std::rc::Rc;

use crate::http::error::HttpError;
use crate::http::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use crate::service::{Ctx, Middleware, Service};
use crate::web::{WebRequest, WebResponse};

/// `Middleware` for setting default response headers.
///
/// This middleware does not set header if response headers already contains it.
/// A header name configured with several values gets all of them when the
/// response does not contain that name.
///
/// ```rust
/// use ntex::http;
/// use ntex::web::{self, middleware, App, HttpResponse};
///
/// fn main() {
///     let app = App::default()
///         .middleware(middleware::DefaultHeaders::new().header("X-Version", "0.2"))
///         .service(
///             web::resource("/test")
///                 .route(web::get().to(async || { HttpResponse::Ok() }))
///                 .route(web::method(http::Method::HEAD).to(async || { HttpResponse::MethodNotAllowed() }))
///         );
/// }
/// ```
#[derive(Debug)]
pub struct DefaultHeaders {
    inner: Rc<Inner>,
}

#[derive(Clone, Debug)]
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
    #[must_use]
    /// Construct `DefaultHeaders` middleware.
    pub fn new() -> DefaultHeaders {
        DefaultHeaders::default()
    }

    #[must_use]
    /// Set a header.
    ///
    /// Calling it again with the same name adds another value.
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
                    Rc::make_mut(&mut self.inner).headers.append(key, value);
                }
                Err(_) => panic!("Cannot create header value"),
            },
            Err(_) => panic!("Cannot create header name"),
        }
        self
    }

    #[must_use]
    /// Set *CONTENT-TYPE* header if response does not contain this header.
    ///
    /// The header is set to `application/octet-stream`.
    pub fn content_type(mut self) -> Self {
        Rc::make_mut(&mut self.inner).ct = true;
        self
    }
}

impl<S, St> Middleware<S, St> for DefaultHeaders {
    type Service = DefaultHeadersMiddleware<S>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        DefaultHeadersMiddleware {
            service,
            inner: self.inner.clone(),
        }
    }
}

#[derive(Debug)]
pub struct DefaultHeadersMiddleware<S> {
    service: S,
    inner: Rc<Inner>,
}

impl<S, St, In> Service<St, WebRequest<In>> for DefaultHeadersMiddleware<S>
where
    S: Service<St, WebRequest<In>, Res = WebResponse>,
{
    type Res = WebResponse;
    type Error = S::Error;

    crate::forward!(St, service);

    async fn call(&self, r: WebRequest<In>, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, S::Error> {
        let mut res = ctx.call(&self.service, r).await?;

        // set response headers
        for key in self.inner.headers.keys() {
            if !res.headers().contains_key(key) {
                for value in self.inner.headers.get_all(key) {
                    res.headers_mut().append(key.clone(), value.clone());
                }
            }
        }
        // default content-type
        if self.inner.ct && !res.headers().contains_key(&CONTENT_TYPE) {
            res.headers_mut().insert(
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
        Ok(res)
    }
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use std::convert::Infallible;

    use super::*;
    use crate::web::{HttpResponse, test::TestRequest, test::ok_service};
    use crate::{Pipeline, fn_service, util::lazy};

    /// STATEMENT: Default headers fill missing response headers without replacing existing values.
    ///
    /// HOW:
    /// - Check middleware readiness/shutdown and assert an absent content type receives `0001`.
    /// - Wrap a service already returning `0002` and assert that value is preserved.
    #[crate::rt_test]
    async fn test_default_headers() {
        let mw = Pipeline::new(
            (),
            Middleware::create(
                &DefaultHeaders::new().header(CONTENT_TYPE, "0001"),
                &(),
                ok_service(),
            ),
        );

        assert!(lazy(|cx| mw.poll_ready(cx).is_ready()).await);
        assert!(lazy(|cx| mw.poll_shutdown(cx).is_ready()).await);

        let req = TestRequest::default().to_srv_request();
        let resp = mw.call(req).await.unwrap();
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "0001");

        let req = TestRequest::default().to_srv_request();
        let srv = fn_service(async move |req: WebRequest<()>| {
            Ok::<_, Infallible>(
                req.into_response(HttpResponse::Ok().header(CONTENT_TYPE, "0002").build()),
            )
        });
        let mw = Pipeline::new(
            (),
            Middleware::create(
                &DefaultHeaders::new().header(CONTENT_TYPE, "0001"),
                &(),
                srv,
            ),
        );
        let resp = mw.call(req).await.unwrap();
        assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "0002");
    }

    /// STATEMENT: A default header configured with several values emits all of them
    /// unless the response already has that name, and configuring after `create`
    /// does not affect the created middleware.
    ///
    /// HOW:
    /// - Configure `x-multi` with `one` and `two`, create the middleware, then add
    ///   `three` to the same builder.
    /// - Call a service without `x-multi` and assert exactly `one`, `two` are emitted.
    /// - Call a service returning `x-multi: own` and assert only `own` remains.
    #[crate::rt_test]
    async fn test_multi_value_header() {
        let dh = DefaultHeaders::new()
            .header("x-multi", "one")
            .header("x-multi", "two");
        let mw = Pipeline::new((), Middleware::create(&dh, &(), ok_service()));
        let _ = dh.header("x-multi", "three");

        let resp = mw
            .call(TestRequest::default().to_srv_request())
            .await
            .unwrap();
        let values: Vec<_> = resp.headers().get_all("x-multi").collect();
        assert_eq!(values, ["one", "two"]);

        let srv = fn_service(async move |req: WebRequest<()>| {
            Ok::<_, Infallible>(
                req.into_response(HttpResponse::Ok().header("x-multi", "own").build()),
            )
        });
        let mw = Pipeline::new(
            (),
            Middleware::create(
                &DefaultHeaders::new()
                    .header("x-multi", "one")
                    .header("x-multi", "two"),
                &(),
                srv,
            ),
        );
        let resp = mw
            .call(TestRequest::default().to_srv_request())
            .await
            .unwrap();
        let values: Vec<_> = resp.headers().get_all("x-multi").collect();
        assert_eq!(values, ["own"]);
    }

    /// STATEMENT: Configuring an invalid default header name panics with a descriptive error.
    ///
    /// HOW:
    /// - Pass a space-containing header name to `DefaultHeaders::header`.
    /// - Require a panic containing `Cannot create header name`.
    #[crate::rt_test]
    #[should_panic(expected = "Cannot create header name")]
    async fn test_invalid_header_name() {
        DefaultHeaders::new().header("no existing header name", "0001");
    }

    /// STATEMENT: Configuring an invalid default header value panics with a descriptive error.
    ///
    /// HOW:
    /// - Pass a newline as the content-type header value.
    /// - Require a panic containing `Cannot create header value`.
    #[crate::rt_test]
    #[should_panic(expected = "Cannot create header value")]
    async fn test_invalid_header_value() {
        DefaultHeaders::new().header(CONTENT_TYPE, "\n");
    }

    /// STATEMENT: Enabling the default content type supplies octet-stream when the service omits it.
    ///
    /// HOW:
    /// - Wrap an OK service with `DefaultHeaders::content_type`.
    /// - Call it and assert `Content-Type` is `application/octet-stream`.
    #[crate::rt_test]
    async fn test_content_type() {
        let srv = fn_service(async move |req: WebRequest<()>| {
            Ok::<_, Infallible>(req.into_response(HttpResponse::Ok().build()))
        });
        let mw = Pipeline::new(
            (),
            Middleware::create(&DefaultHeaders::new().content_type(), &(), srv),
        );

        let req = TestRequest::default().to_srv_request();
        let resp = mw.call(req).await.unwrap();
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
    }
}
