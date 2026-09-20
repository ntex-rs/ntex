#![allow(clippy::new_without_default)]
use std::marker::PhantomData;

use crate::error::{Failure, IntoFailure};
use crate::http::{Request, Response};
use crate::router::ResourceDef;
use crate::service::{Identity, ServiceChainFactory, map_state_factory};
use crate::{Cfg, IntoServiceFactory, Middleware, Service, ServiceFactory, factory};

use super::app_service::{AppFactory, WebServiceRouter};
use super::config::{ServiceConfig, WebAppConfig};
use super::error::{WebError, WebResponseError};
use super::service::{AppServiceFactory, ServiceFactoryWrapper, WebServiceFactory};
use super::stack::{Filter, WebStack};
use super::{HttpService, Resource, Route, State, WebRequest, WebResponse};

/// Application builder - structure that follows the builder pattern
/// for building application instances.
#[derive(derive_more::Debug)]
#[debug("App")]
pub struct App<St: State, In = (), Out = In, M = Identity, F = Filter<St, In>> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    external: Vec<ResourceDef>,
    config: Option<Cfg<WebAppConfig>>,
    case_insensitive: bool,
    ph: PhantomData<Out>,
}

/// Application builder - structure that follows the builder pattern
/// for building application instances.
#[derive(derive_more::Debug)]
#[debug("AppServices")]
pub struct AppServices<St: State, In, Out, M, F> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    services: Vec<Box<dyn AppServiceFactory<St, Out>>>,
    default: Option<HttpService<St, Out>>,
    external: Vec<ResourceDef>,
    config: Option<Cfg<WebAppConfig>>,
    case_insensitive: bool,
    ph: PhantomData<In>,
}

impl Default for App<()> {
    fn default() -> Self {
        App {
            middleware: Identity,
            filter: factory(Filter::new()),
            config: None,
            external: Vec::new(),
            case_insensitive: false,
            ph: PhantomData,
        }
    }
}

impl<St: State, In> App<St, In, In> {
    /// Create application builder. Application can be configured with a builder-like pattern.
    #[must_use]
    pub fn new() -> Self {
        App {
            middleware: Identity,
            filter: factory(Filter::new()),
            config: None,
            external: Vec::new(),
            case_insensitive: false,
            ph: PhantomData,
        }
    }
}

impl<St, In, Out, M, F> App<St, In, Out, M, F>
where
    St: State,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
{
    /// Run external configuration as part of the application building
    /// process.
    ///
    /// This function is useful for moving parts of configuration to a
    /// different module or even library. For example,
    /// some of the resource's configuration could be moved to different module.
    ///
    /// ```rust,ignore
    /// use ntex::web::{self, middleware, App, HttpResponse};
    ///
    /// // this function could be located in different module
    /// fn config(cfg: &mut web::ServiceConfig) {
    ///     cfg.service(web::resource("/test")
    ///         .route(web::get().to(async || { HttpResponse::Ok() }))
    ///         .route(web::head().to(async || { HttpResponse::MethodNotAllowed() }))
    ///     );
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .middleware(middleware::Logger::default())
    ///         .configure(config)  // <- register resources
    ///         .route("/index.html", web::get().to(async || { HttpResponse::Ok() }));
    /// }
    /// ```
    #[must_use]
    pub fn configure(
        self,
        f: impl FnOnce(&mut ServiceConfig<St, Out>),
    ) -> AppServices<St, In, Out, M, F> {
        let mut cfg = ServiceConfig::new(self.external);
        f(&mut cfg);

        AppServices {
            services: cfg.services,
            default: None,
            filter: self.filter,
            middleware: self.middleware,
            config: self.config,
            external: cfg.external,
            case_insensitive: self.case_insensitive,
            ph: PhantomData,
        }
    }

    /// Register a route for an application path.
    ///
    /// This is shorthand for creating a [`Resource`] with one route and
    /// registering it with [`App::service()`]. The route's method and custom
    /// guards are promoted to resource guards.
    ///
    /// Each call creates a separate resource, so the same path can be
    /// registered more than once with different guards.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default()
    ///     .route("/items", web::get().to(async || "list"))
    ///     .route("/items", web::post().to(async || HttpResponse::Created()));
    /// ```
    #[must_use]
    pub fn route(self, path: &str, mut route: Route<St, Out>) -> AppServices<St, In, Out, M, F> {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Registers a web service with the application.
    ///
    /// A service defines its own path and guards through [`WebServiceFactory`].
    /// Common services include [`Resource`], [`Scope`], handlers created with
    /// route attribute macros, and custom services built with `web::service()`.
    ///
    /// Use a resource to group several routes, filters, middleware, or a
    /// fallback under one path. Use a scope to group services under a shared
    /// path prefix.
    ///
    /// If no registered service matches the request path and guards, the
    /// application's default service is used.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default()
    ///     .service(
    ///         web::resource("/users")
    ///             .route(web::get().to(async || "users"))
    ///             .route(web::post().to(async || HttpResponse::Created())),
    ///     )
    ///     .service(
    ///         web::scope("/api")
    ///             .route("/health", web::get().to(async || "OK")),
    ///     );
    /// ```
    #[must_use]
    pub fn service<S>(self, factory: S) -> AppServices<St, In, Out, M, F>
    where
        S: WebServiceFactory<St, Out> + 'static,
    {
        AppServices {
            services: vec![Box::new(ServiceFactoryWrapper::new(factory))],
            default: None,
            filter: self.filter,
            middleware: self.middleware,
            config: self.config,
            external: self.external,
            case_insensitive: self.case_insensitive,
            ph: PhantomData,
        }
    }

    /// Set the fallback service for unmatched application requests.
    ///
    /// The fallback is called when no top-level resource or scope matches the
    /// request path and guards. Without a custom fallback, the application
    /// returns `404 Not Found`.
    ///
    /// A matched resource or scope handles its own routing failures, so its
    /// requests do not fall through to this service.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse};
    ///
    /// async fn not_found(req: HttpRequest) -> HttpResponse {
    ///     HttpResponse::NotFound()
    ///         .body(format!("No resource for {}", req.path()))
    /// }
    ///
    /// App::default()
    ///     .route("/health", web::get().to(async || "ready"))
    ///     .default_service(web::to(not_found));
    /// ```
    #[must_use]
    pub fn default_service<U>(
        self,
        f: impl IntoServiceFactory<U, St, WebRequest<Out>>,
    ) -> AppServices<St, In, Out, M, F>
    where
        U: ServiceFactory<St, WebRequest<Out>, Res = WebResponse> + 'static,
        U::Error: WebResponseError<St, St::Error>,
        U::InitError: IntoFailure,
    {
        // create and configure default resource
        let default = Some(HttpService::new(
            f.into_factory()
                .map_err(WebError::from_err)
                .map_init_err(IntoFailure::fail),
        ));

        AppServices {
            default,
            services: Vec::new(),
            filter: self.filter,
            middleware: self.middleware,
            config: self.config,
            external: self.external,
            case_insensitive: self.case_insensitive,
            ph: PhantomData,
        }
    }

    /// Register an external resource.
    ///
    /// External resources are useful for URL generation purposes only
    /// and are never considered for matching at request time. Calls to
    /// `HttpRequest::url_for()` will work as expected.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse, error::UrlGenerationError};
    ///
    /// async fn index(req: HttpRequest) -> Result<HttpResponse, UrlGenerationError> {
    ///     let url = req.url_for("youtube", &["asdlkjqme"])?;
    ///     assert_eq!(url.as_str(), "https://youtube.com/watch/asdlkjqme");
    ///     Ok(HttpResponse::Ok().into())
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .external_resource("youtube", "https://youtube.com/watch/{video_id}")
    ///         .service(web::resource("/index.html").route(
    ///             web::get().to(index)));
    /// }
    /// ```
    #[must_use]
    pub fn external_resource(mut self, name: impl AsRef<str>, url: impl AsRef<str>) -> Self {
        let mut rdef = ResourceDef::new(url.as_ref());
        *rdef.name_mut() = name.as_ref().to_string();
        self.external.push(rdef);
        self
    }

    /// Set the application's runtime configuration.
    ///
    /// [`WebAppConfig`] contains connection metadata used by the application,
    /// such as the host, secure-connection flag, local address, and request pool
    /// size. It can also store typed configuration values with
    /// [`WebAppConfig::set_state()`]; those values are available through
    /// [`HttpRequest::app_state()`] and [`WebRequest::app_state()`].
    ///
    /// Without an explicit configuration, each request uses the
    /// [`WebAppConfig`] from its I/O context, or the default configuration if
    /// the request has no associated I/O object. This method overrides that
    /// selection for every request handled by this application.
    ///
    /// This configuration is separate from the service-level application state
    /// represented by `St`. To register routes and services from an external
    /// function, use [`App::configure()`] instead.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, WebAppConfig};
    ///
    /// async fn index(req: HttpRequest) -> String {
    ///     let value = req.app_state::<usize>().copied().unwrap_or_default();
    ///     format!("Configured value: {value}")
    /// }
    ///
    /// let config = WebAppConfig::new()
    ///     .set_host("www.example.com".to_owned())
    ///     .set_secure()
    ///     .set_state(42usize);
    ///
    /// App::default()
    ///     .with_config(config)
    ///     .route("/", web::get().to(index));
    /// ```
    #[must_use]
    pub fn with_config(mut self, cfg: impl Into<Cfg<WebAppConfig>>) -> Self {
        self.config = Some(cfg.into());
        self
    }

    /// Registers a request filter.
    ///
    /// Application filters run before the application router selects a
    /// resource or scope. Filters are called in registration order, and each
    /// filter receives the [`WebRequest`] returned by the previous one.
    ///
    /// A filter can inspect or modify the request, or use
    /// [`WebRequest::map_state()`] to change its request-local state type. It
    /// must return another `WebRequest` to continue processing. Returning an
    /// error stops the filter chain and prevents routing; the error is handled
    /// through [`WebResponseError`].
    ///
    /// Application middleware wraps the filter and router, so middleware runs
    /// before filters on the inbound path.
    ///
    /// ```rust
    /// use std::convert::Infallible;
    /// use ntex::web::{self, App, WebRequest};
    ///
    /// async fn authenticate(
    ///     req: WebRequest<()>,
    /// ) -> Result<WebRequest<&'static str>, Infallible> {
    ///     Ok(req.map_state(|()| "alice"))
    /// }
    ///
    /// async fn index(_state: &(), user: &'static str) -> String {
    ///     format!("Hello, {user}!")
    /// }
    ///
    /// App::new()
    ///     .filter(authenticate)
    ///     .route("/", web::get().to_with_state(index));
    /// ```
    #[must_use]
    pub fn filter<Sf, R>(
        self,
        filter: impl IntoServiceFactory<Sf, St, WebRequest<Out>>,
    ) -> App<
        St,
        In,
        R,
        M,
        impl ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<R>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    >
    where
        Sf: ServiceFactory<St, WebRequest<Out>, Res = WebRequest<R>>,
        Sf::Error: WebResponseError<St, St::Error>,
        Sf::InitError: IntoFailure,
    {
        App {
            filter: self.filter.and_then(
                filter
                    .into_factory()
                    .map_err(WebError::from_err)
                    .map_init_err(IntoFailure::fail),
            ),
            middleware: self.middleware,
            config: self.config,
            external: self.external,
            case_insensitive: self.case_insensitive,
            ph: PhantomData,
        }
    }

    /// Registers a middleware for this application.
    ///
    /// Use application middleware for work that should apply to every request,
    /// such as logging, response headers, or authentication. It runs before
    /// the application filter and router on the way in, and can inspect or
    /// modify the response on the way back.
    ///
    /// Middleware may also return a response without calling the service it
    /// wraps. In that case, the rest of the application pipeline is skipped.
    ///
    /// Requests pass through middleware in the order it was added. Responses
    /// travel back in the opposite order. In this example, `DefaultHeaders`
    /// sees the request before `Logger`, while `Logger` sees the response
    /// before `DefaultHeaders`.
    ///
    /// Custom middleware should call the wrapped service through
    /// [`Ctx::call()`] so readiness and lifecycle events are handled
    /// correctly.
    ///
    /// ```rust
    /// use ntex::web::{self, middleware, App};
    ///
    /// App::default()
    ///     .middleware(
    ///         middleware::DefaultHeaders::new()
    ///             .header("x-application", "example"),
    ///     )
    ///     .middleware(middleware::Logger::default())
    ///     .route("/", web::get().to(async || "Hello"));
    /// ```
    #[must_use]
    pub fn middleware<U>(self, mw: U) -> App<St, In, Out, WebStack<St, U, M>, F> {
        App {
            middleware: WebStack::new(mw, self.middleware),
            filter: self.filter,
            config: self.config,
            external: self.external,
            case_insensitive: self.case_insensitive,
            ph: PhantomData,
        }
    }

    #[must_use]
    /// Use ascii case-insensitive routing.
    ///
    /// Only static segments could be case-insensitive.
    pub fn case_insensitive_routing(mut self) -> Self {
        self.case_insensitive = true;
        self
    }
}

impl<St, In, Out, M, F> AppServices<St, In, Out, M, F>
where
    St: State,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
{
    /// Register a route for an application path.
    ///
    /// This is shorthand for creating a [`Resource`] with one route and
    /// registering it with [`AppServices::service()`]. The route's method and
    /// custom guards are promoted to resource guards.
    ///
    /// Consequently, if those guards reject a request, the generated resource
    /// does not match and the application router continues searching. If
    /// nothing else matches, the application default service is used. To use a
    /// resource-level fallback such as the built-in `405 Method Not Allowed`,
    /// register an explicit [`Resource`] and add routes with
    /// [`Resource::route()`].
    ///
    /// Each call creates a separate resource, so the same path can be
    /// registered more than once with different guards.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default()
    ///     .route("/items", web::get().to(async || "list"))
    ///     .route(
    ///         "/items",
    ///         web::post().to(async || HttpResponse::Created()),
    ///     );
    /// ```
    #[must_use]
    pub fn route(self, path: &str, mut route: Route<St, Out>) -> Self {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Registers another web service with the application.
    ///
    /// This has the same behavior as [`App::service()`]. The service supplies
    /// its own path and guards and becomes part of the application's top-level
    /// router.
    ///
    /// If none of the registered services match, the application's default
    /// service is used.
    #[must_use]
    pub fn service<S>(mut self, factory: S) -> Self
    where
        S: WebServiceFactory<St, Out> + 'static,
    {
        self.services
            .push(Box::new(ServiceFactoryWrapper::new(factory)));
        self
    }

    /// Set the fallback service for unmatched application requests.
    ///
    /// The fallback is called when no top-level resource or scope matches the
    /// request path and guards. Without a custom fallback, the application
    /// returns `404 Not Found`.
    ///
    /// A matched resource or scope handles its own routing failures, so its
    /// requests do not fall through to this service.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse};
    ///
    /// async fn not_found(req: HttpRequest) -> HttpResponse {
    ///     HttpResponse::NotFound()
    ///         .body(format!("No resource for {}", req.path()))
    /// }
    ///
    /// App::default()
    ///     .route("/health", web::get().to(async || "ready"))
    ///     .default_service(web::to(not_found));
    /// ```
    #[must_use]
    pub fn default_service<U>(mut self, f: impl IntoServiceFactory<U, St, WebRequest<Out>>) -> Self
    where
        U: ServiceFactory<St, WebRequest<Out>, Res = WebResponse> + 'static,
        U::Error: WebResponseError<St, St::Error>,
        U::InitError: IntoFailure,
    {
        // create and configure default resource
        self.default = Some(HttpService::new(
            f.into_factory()
                .map_err(WebError::from_err)
                .map_init_err(IntoFailure::fail),
        ));

        self
    }
}

impl<St, In, Out, M, F> AppServices<St, In, Out, M, F>
where
    St: State,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    /// Builds the application into a service factory.
    ///
    /// The returned factory accepts [`Request`] values and uses application
    /// state supplied by the surrounding service pipeline. It can be passed to
    /// [`HttpService`] when building an HTTP server manually.
    ///
    /// Applications passed to [`web::server()`] do not normally need an
    /// explicit call to `build()`.
    ///
    /// ```rust
    /// use ntex::web;
    ///
    /// let factory = web::App::default()
    ///     .route("/", web::get().to(async || "Hello"))
    ///     .build();
    /// ```
    ///
    /// [`HttpService`]: crate::http::HttpService
    /// [`web::server()`]: super::server
    pub fn build(
        self,
    ) -> impl ServiceFactory<
        St,
        Request,
        Res = Response,
        Error = WebError<St, St::Error>,
        InitError = Failure,
    > {
        IntoServiceFactory::<AppFactory<St, In, Out, M, F>, St, Request>::into_factory(self)
    }
}

impl<St, In, Out, M, F> AppServices<St, In, Out, M, F>
where
    St: State,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    /// Builds the application with a fixed application state.
    ///
    /// Unlike [`AppServices::build()`], which takes its application state from
    /// the surrounding service pipeline, this method stores `state` in the
    /// returned factory. Web handlers, filters, middleware, and services use
    /// this fixed state even when the outer pipeline uses a different state
    /// type.
    ///
    /// ```rust
    /// use ntex::web;
    ///
    /// #[derive(Clone)]
    /// struct AppState {
    ///     greeting: &'static str,
    /// }
    ///
    /// impl web::State for AppState {
    ///     type Error = web::DefaultError;
    /// }
    ///
    /// async fn index(state: &AppState, _request_state: ()) -> String {
    ///     state.greeting.to_owned()
    /// }
    ///
    /// let app = web::App::<AppState>::new()
    ///     .route("/", web::get().to_with_state(index))
    ///     .build_with::<()>(AppState { greeting: "Hello" });
    /// ```
    pub fn build_with<Outer>(
        self,
        state: St,
    ) -> impl ServiceFactory<
        Outer,
        Request,
        Res = Response,
        Error = WebError<St, St::Error>,
        InitError = Failure,
    >
    where
        St: Clone,
    {
        map_state_factory(
            state,
            IntoServiceFactory::<AppFactory<St, In, Out, M, F>, St, Request>::into_factory(self),
        )
    }
}

impl<St, In, Out, M, F> IntoServiceFactory<AppFactory<St, In, Out, M, F>, St, Request>
    for AppServices<St, In, Out, M, F>
where
    St: State,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    fn into_factory(self) -> AppFactory<St, In, Out, M, F> {
        AppFactory::new(
            self.middleware,
            self.filter,
            self.services,
            self.default,
            self.config,
            self.external,
            self.case_insensitive,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, convert::Infallible, rc::Rc};

    use super::*;
    use crate::http::{Method, StatusCode, header, header::HeaderValue};
    use crate::web::test::{TestRequest, call_service, init_service, read_body};
    use crate::web::{self, HttpRequest, HttpResponse, middleware::DefaultHeaders};

    #[crate::rt_test]
    async fn test_default_resource() {
        let srv = App::new()
            .service(web::resource("/test").to(async || HttpResponse::Ok()))
            .build()
            .pipeline(())
            .await
            .unwrap();
        let req = TestRequest::with_uri("/test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/blah").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let srv = App::new()
            .service(web::resource("/test").to(async || HttpResponse::Ok()))
            .service(
                web::resource("/test2")
                    .default_service(async move |r: WebRequest<()>| {
                        Ok::<_, Infallible>(r.into_response(HttpResponse::Created()))
                    })
                    .route(web::get().to(async || HttpResponse::Ok())),
            )
            .default_service(async move |r: WebRequest<()>| {
                Ok::<_, Infallible>(r.into_response(HttpResponse::MethodNotAllowed()))
            })
            .build()
            .pipeline(())
            .await
            .unwrap();

        let req = TestRequest::with_uri("/blah").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let req = TestRequest::with_uri("/test2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test2")
            .method(Method::POST)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[crate::rt_test]
    async fn test_filter() {
        let filter = Rc::new(Cell::new(false));
        let filter2 = filter.clone();
        let srv = init_service(
            App::new()
                .filter(async move |req: WebRequest<()>| {
                    filter2.set(true);
                    Ok::<_, Infallible>(req)
                })
                .route("/test", web::get().to(async || HttpResponse::Ok())),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(filter.get());
    }

    #[crate::rt_test]
    async fn test_wrap() {
        let srv = init_service(
            App::new()
                .middleware(
                    DefaultHeaders::new()
                        .header(header::CONTENT_TYPE, HeaderValue::from_static("0001")),
                )
                .route("/test", web::get().to(async || HttpResponse::Ok())),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[crate::rt_test]
    async fn test_router_wrap() {
        let srv = init_service(
            App::new()
                .middleware(
                    DefaultHeaders::new()
                        .header(header::CONTENT_TYPE, HeaderValue::from_static("0001")),
                )
                .route("/test", web::get().to(async || HttpResponse::Ok())),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[crate::rt_test]
    async fn test_case_insensitive_router() {
        let srv = init_service(
            App::new()
                .case_insensitive_routing()
                .route("/test", web::get().to(async || HttpResponse::Ok())),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/Test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_extension() {
        let cfg = WebAppConfig::new().set_state(10usize);

        let srv = init_service(
            App::new()
                .with_config(cfg)
                .filter(async move |req: WebRequest<()>| {
                    assert_eq!(*req.app_state::<usize>().unwrap(), 10);
                    Ok::<_, Infallible>(req)
                })
                .service(web::resource("/").to(async move |req: HttpRequest| {
                    assert_eq!(*req.app_state::<usize>().unwrap(), 10);
                    HttpResponse::Ok()
                })),
        )
        .await;

        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[cfg(feature = "url")]
    #[crate::rt_test]
    async fn test_external_resource() {
        use crate::util::Bytes;

        let srv = init_service(
            App::new()
                .external_resource("youtube", "https://youtube.com/watch/{video_id}")
                .route(
                    "/test",
                    web::get().to(async move |req: HttpRequest| {
                        HttpResponse::Ok()
                            .body(format!("{}", req.url_for("youtube", ["12345"]).unwrap()))
                    }),
                ),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        assert_eq!(body, Bytes::from_static(b"https://youtube.com/watch/12345"));
    }
}
