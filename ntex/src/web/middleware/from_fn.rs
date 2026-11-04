use std::{fmt, marker::PhantomData};

use crate::service::{BindCtx, Ctx, Middleware, Service};
use crate::web::{State, WebError, WebRequest, WebResponse, WebResponseError};

/// Creates web middleware from an async closure.
///
/// The closure receives the request and a [`Next`] handle. Call
/// [`Next::call()`] to run the rest of the pipeline, or return a response
/// without calling it to short-circuit. Binding, readiness, and shutdown are
/// forwarded to the inner service automatically.
///
/// Closure types are inferred, no annotations are needed:
///
/// ```rust,no_run
/// use ntex::http::header::{HeaderName, HeaderValue};
/// use ntex::web::{self, App, HttpResponse, middleware::from_fn};
///
/// let server = web::HttpServer::new(async |_| {
///     App::new()
///         .middleware(from_fn(async |req, next| {
///             if !req.headers().contains_key("authorization") {
///                 return Ok(req.into_response(HttpResponse::Unauthorized().build()));
///             }
///             let mut res = next.call(req).await?;
///             res.headers_mut().insert(
///                 HeaderName::from_static("x-application"),
///                 HeaderValue::from_static("example"),
///             );
///             Ok(res)
///         }))
///         .route("/", web::get().to(async || "Hello"))
/// });
/// ```
///
/// Return `Err(web::WebError::from_err(err))` to reject a request with any
/// [`WebResponseError`]. To read application state with [`Next::st()`],
/// annotate the state type of `next`:
///
/// ```rust
/// use ntex::http::header::{HeaderName, HeaderValue};
/// use ntex::web::{self, App, AppState, middleware::{Next, from_fn}, test};
///
/// # #[ntex::main]
/// # async fn main() {
/// let app = test::init_service_st(
///     AppState::new("example"),
///     App::new()
///         .middleware(from_fn(async |req, next: Next<'_, _, AppState<&str>>| {
///             let name = **next.st();
///             let mut res = next.call(req).await?;
///             res.headers_mut().insert(
///                 HeaderName::from_static("x-application"),
///                 HeaderValue::from_static(name),
///             );
///             Ok(res)
///         }))
///         .route("/", web::get().to(async || "Hello")),
/// )
/// .await;
/// # }
/// ```
pub fn from_fn<F, S, St, In>(f: F) -> FromFn<F, S, St, In>
where
    St: State,
    F: AsyncFn(WebRequest<In>, Next<'_, S, St>) -> Result<WebResponse, WebError<St, St::Error>>
        + Clone,
{
    FromFn { f, _t: PhantomData }
}

/// Middleware created by [`from_fn`].
pub struct FromFn<F, S, St, In = ()> {
    f: F,
    _t: PhantomData<fn(S, St, In)>,
}

impl<F: Clone, S, St, In> Clone for FromFn<F, S, St, In> {
    fn clone(&self) -> Self {
        FromFn {
            f: self.f.clone(),
            _t: PhantomData,
        }
    }
}

impl<F, S, St, In> fmt::Debug for FromFn<F, S, St, In> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FromFn")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, S, St, In> Middleware<S, St> for FromFn<F, S, St, In>
where
    St: State,
    S: Service<St, WebRequest<In>, Res = WebResponse>,
    S::Error: WebResponseError<St, St::Error>,
    F: AsyncFn(WebRequest<In>, Next<'_, S, St>) -> Result<WebResponse, WebError<St, St::Error>>
        + Clone,
{
    type Service = FromFnService<F, S>;

    fn create(&self, _: &St, svc: S) -> Self::Service {
        FromFnService {
            f: self.f.clone(),
            svc,
        }
    }
}

/// Service created by [`FromFn`].
pub struct FromFnService<F, S> {
    f: F,
    svc: S,
}

impl<F, S: fmt::Debug> fmt::Debug for FromFnService<F, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FromFnService")
            .field("svc", &self.svc)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, S, St, In> Service<St, WebRequest<In>> for FromFnService<F, S>
where
    St: State,
    S: Service<St, WebRequest<In>, Res = WebResponse>,
    S::Error: WebResponseError<St, St::Error>,
    F: AsyncFn(WebRequest<In>, Next<'_, S, St>) -> Result<WebResponse, WebError<St, St::Error>>,
{
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    async fn call(
        &self,
        req: WebRequest<In>,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        ctx.call(&NextService(&self.svc), (req, &self.f)).await
    }

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind::<_, St, _>(&self.svc);
    }

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(&self.svc).await.map_err(WebError::from_err)
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        ctx.shutdown(&self.svc).await;
    }
}

/// Handle to the rest of the pipeline, passed to [`from_fn`] closures.
pub struct Next<'a, S, St> {
    svc: &'a S,
    ctx: Ctx<'a, NextService<'a, S>, St>,
}

impl<S, St> fmt::Debug for Next<'_, S, St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Next").finish()
    }
}

impl<S, St: State> Next<'_, S, St> {
    /// Calls the inner service.
    ///
    /// Errors of the inner service are converted to [`WebError`].
    pub async fn call<In>(
        &self,
        req: WebRequest<In>,
    ) -> Result<WebResponse, WebError<St, St::Error>>
    where
        S: Service<St, WebRequest<In>, Res = WebResponse>,
        S::Error: WebResponseError<St, St::Error>,
    {
        self.ctx
            .call(self.svc, req)
            .await
            .map_err(WebError::from_err)
    }

    /// Returns the application state.
    pub fn st(&self) -> &St {
        self.ctx.st()
    }
}

struct NextService<'a, S>(&'a S);

impl<'f, F, S, St, In> Service<St, (WebRequest<In>, &'f F)> for NextService<'_, S>
where
    St: State,
    S: Service<St, WebRequest<In>, Res = WebResponse>,
    S::Error: WebResponseError<St, St::Error>,
    F: AsyncFn(WebRequest<In>, Next<'_, S, St>) -> Result<WebResponse, WebError<St, St::Error>>,
{
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    async fn call(
        &self,
        (req, f): (WebRequest<In>, &'f F),
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        f(req, Next { svc: self.0, ctx }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::StatusCode;
    use crate::http::header::{CONTENT_TYPE, HeaderName, HeaderValue};
    use crate::web::test::{TestRequest, call_service, init_service, init_service_st};
    use crate::web::{self, App, AppState, HttpResponse, error, middleware::DefaultHeaders};

    /// STATEMENT: `from_fn` closures infer their types without annotations at app,
    /// scope and resource level, and compose with other middlewares.
    ///
    /// HOW:
    /// - Register unannotated `from_fn` closures on `App`, `Scope` and `Resource`,
    ///   stacked with `DefaultHeaders`.
    /// - Confirm each layer's header is set and `/skip` is short-circuited with 403.
    #[crate::rt_test]
    async fn from_fn_infers_and_stacks() {
        let srv = init_service(
            App::new()
                .middleware(from_fn(async |req, next| {
                    if req.path() == "/skip" {
                        return Ok(req.into_response(HttpResponse::Forbidden().build()));
                    }
                    let mut res = next.call(req).await?;
                    res.headers_mut()
                        .insert(CONTENT_TYPE, HeaderValue::from_static("app"));
                    Ok(res)
                }))
                .middleware(DefaultHeaders::new().header("x-a", "1"))
                .middleware(from_fn(async |req, next| next.call(req).await))
                .service(
                    web::scope("/s")
                        .middleware(from_fn(async |req, next| {
                            let mut res = next.call(req).await?;
                            res.headers_mut().insert(
                                HeaderName::from_static("x-scope"),
                                HeaderValue::from_static("1"),
                            );
                            Ok(res)
                        }))
                        .service(
                            web::resource("/r")
                                .middleware(from_fn(async |req, next| {
                                    let mut res = next.call(req).await?;
                                    res.headers_mut().insert(
                                        HeaderName::from_static("x-res"),
                                        HeaderValue::from_static("1"),
                                    );
                                    Ok(res)
                                }))
                                .to(async || HttpResponse::Ok()),
                        ),
                )
                .route("/test", web::get().to(async || HttpResponse::Ok())),
        )
        .await;
        let res = call_service(&srv, TestRequest::with_uri("/test").to_request()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get(CONTENT_TYPE).unwrap(), "app");
        let res = call_service(&srv, TestRequest::with_uri("/skip").to_request()).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let res = call_service(&srv, TestRequest::with_uri("/s/r").to_request()).await;
        assert_eq!(res.headers().get("x-scope").unwrap(), "1");
        assert_eq!(res.headers().get("x-res").unwrap(), "1");
        assert_eq!(res.headers().get("x-a").unwrap(), "1");
    }

    /// STATEMENT: A `from_fn` closure can reject a request by returning a `WebError`.
    ///
    /// HOW:
    /// - Return `Err(WebError::from_err(ErrorUnauthorized(..)))` when the
    ///   `authorization` header is missing, otherwise call `next`.
    /// - Confirm 401 without the header and 200 with it.
    #[crate::rt_test]
    async fn from_fn_returns_error() {
        let srv = init_service(
            App::new()
                .middleware(from_fn(async |req, next| {
                    if !req.headers().contains_key("authorization") {
                        return Err(WebError::from_err(error::ErrorUnauthorized("no auth")));
                    }
                    next.call(req).await
                }))
                .route("/test", web::get().to(async || HttpResponse::Ok())),
        )
        .await;
        let res = call_service(&srv, TestRequest::with_uri("/test").to_request()).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let req = TestRequest::with_uri("/test")
            .header("authorization", "x")
            .to_request();
        let res = call_service(&srv, req).await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    /// STATEMENT: A `from_fn` closure can read application state through `Next::st`.
    ///
    /// HOW:
    /// - Build an app with `AppState<&str>` and annotate `next: Next<'_, _, AppState<&str>>`.
    /// - Copy the state value into a response header and confirm it.
    #[crate::rt_test]
    async fn from_fn_reads_state() {
        let srv = init_service_st(
            AppState::new("st"),
            App::<AppState<&'static str>>::new()
                .middleware(from_fn(async |req, next: Next<'_, _, AppState<&str>>| {
                    let tag = **next.st();
                    let mut res = next.call(req).await?;
                    res.headers_mut().insert(
                        HeaderName::from_static("x-st"),
                        HeaderValue::from_static(tag),
                    );
                    Ok(res)
                }))
                .route("/test", web::get().to(async || HttpResponse::Ok())),
        )
        .await;
        let res = call_service(&srv, TestRequest::with_uri("/test").to_request()).await;
        assert_eq!(res.headers().get("x-st").unwrap(), "st");
    }
}
