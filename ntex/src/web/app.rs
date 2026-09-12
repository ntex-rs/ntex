#![allow(clippy::new_without_default)]
use std::marker::PhantomData;

use crate::error::{Failure, IntoFailure};
use crate::http::{Request, Response};
use crate::router::ResourceDef;
use crate::service::{Identity, ServiceChainFactory};
use crate::{Cfg, IntoServiceFactory, Middleware, Service, ServiceFactory, factory};

use super::app_service::{AppFactory, AppRouter};
use super::config::{ServiceConfig, WebAppConfig};
use super::error::{WebError, WebResponseError};
use super::service::{AppServiceFactory, ServiceFactoryWrapper, WebServiceFactory};
use super::stack::{Filter, WebStack};
use super::{AppState, HttpService, Resource, Route, WebRequest, WebResponse};

/// Application builder - structure that follows the builder pattern
/// for building application instances.
#[derive(derive_more::Debug)]
#[debug("App")]
pub struct App<St: AppState, In, Out = In, M = Identity, F = Filter<St, In>> {
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
pub struct AppServices<St: AppState, In, Out, M, F> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    services: Vec<Box<dyn AppServiceFactory<St, Out>>>,
    default: Option<HttpService<St, Out>>,
    external: Vec<ResourceDef>,
    config: Option<Cfg<WebAppConfig>>,
    case_insensitive: bool,
    ph: PhantomData<In>,
}

impl<In> App<(), In, In> {
    #[must_use]
    /// Create application builder. Application can be configured with a builder-like pattern.
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

impl<St: AppState, In> App<St, In, In> {
    #[must_use]
    /// Create application builder. Application can be configured with a builder-like pattern.
    pub fn with() -> Self {
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
    St: AppState,
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
    #[must_use]
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
    ///     let app = App::new()
    ///         .middleware(middleware::Logger::default())
    ///         .configure(config)  // <- register resources
    ///         .route("/index.html", web::get().to(async || { HttpResponse::Ok() }));
    /// }
    /// ```
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

    #[must_use]
    /// Configure route for a specific path.
    ///
    /// This is a simplified version of the `App::service()` method.
    /// This method can be used multiple times with same path, in that case
    /// multiple resources with one route would be registered for same resource path.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// async fn index(data: web::types::Path<(String, String)>) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .route("/test1", web::get().to(index))
    ///         .route("/test2", web::post().to(async || { HttpResponse::MethodNotAllowed() }));
    /// }
    /// ```
    pub fn route(self, path: &str, mut route: Route<St, Out>) -> AppServices<St, In, Out, M, F> {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    #[must_use]
    /// Register http service.
    ///
    /// Http service is any type that implements `WebServiceFactory` trait.
    ///
    /// ntex provides several services implementations:
    ///
    /// * `Resource` is an entry in resource table which corresponds to requested URL.
    /// * `Scope` is a set of resources with common root path.
    /// * `StaticFiles` is a service for static files support
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

    #[must_use]
    /// Default service to be used if no matching resource could be found.
    ///
    /// It is possible to use services like `Resource`, `Route`.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .service(
    ///             web::resource("/index.html").route(web::get().to(index)))
    ///         .default_service(
    ///             web::route().to(async || { HttpResponse::NotFound() }));
    /// }
    /// ```
    ///
    /// It is also possible to use static files as default service.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .service(
    ///             web::resource("/index.html").to(async || { HttpResponse::Ok() }))
    ///         .default_service(
    ///             web::to(async || { HttpResponse::NotFound() })
    ///         );
    /// }
    /// ```
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

    #[must_use]
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
    ///         .service(web::resource("/index.html").route(
    ///             web::get().to(index)))
    ///         .external_resource("youtube", "https://youtube.com/watch/{video_id}");
    /// }
    /// ```
    pub fn external_resource(mut self, name: impl AsRef<str>, url: impl AsRef<str>) -> Self {
        let mut rdef = ResourceDef::new(url.as_ref());
        *rdef.name_mut() = name.as_ref().to_string();
        self.external.push(rdef);
        self
    }

    #[must_use]
    /// Set custom app configuration.
    pub fn config(mut self, cfg: Cfg<WebAppConfig>) -> Self {
        self.config = Some(cfg);
        self
    }

    #[must_use]
    /// Register request filter.
    ///
    /// Filter runs during inbound processing in the request
    /// lifecycle (request -> response), modifying request as
    /// necessary, across all requests managed by the *Application*.
    ///
    /// Use filter when you need to read or modify *every* request in some way.
    /// If filter returns request object then pipeline execution continues
    /// to the next service in pipeline. In case of response, it get returned
    /// immediately.
    ///
    /// ```rust
    /// use ntex::http::header::{CONTENT_TYPE, HeaderValue};
    /// use ntex::web::{self, middleware, App};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .middleware(middleware::Logger::default())
    ///         .route("/index.html", web::get().to(index));
    /// }
    /// ```
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

    #[must_use]
    /// Registers middleware.
    ///
    /// Registers middleware in the form of a middleware component (type),
    /// that runs during inbound and/or outbound processing in the request
    /// lifecycle (request -> response), modifying request/response as
    /// necessary, across all requests managed by the *Application*.
    ///
    /// Use middleware when you need to read or modify *every* request or
    /// response in some way.
    ///
    /// As you register middleware in the App builder, imagine wrapping
    /// layers around an inner App.
    ///
    /// ```rust
    /// use ntex::http::header::{CONTENT_TYPE, HeaderValue};
    /// use ntex::web::{self, middleware, App};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .middleware(middleware::Logger::default())
    ///         .route("/index.html", web::get().to(index));
    /// }
    /// ```
    pub fn middleware<U>(self, mw: U) -> App<St, In, Out, WebStack<St, M, U>, F> {
        App {
            middleware: WebStack::new(self.middleware, mw),
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
    St: AppState,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
{
    #[must_use]
    /// Configure route for a specific path.
    ///
    /// This is a simplified version of the `App::service()` method.
    /// This method can be used multiple times with same path, in that case
    /// multiple resources with one route would be registered for same resource path.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// async fn index(data: web::types::Path<(String, String)>) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .route("/test1", web::get().to(index))
    ///         .route("/test2", web::post().to(async || { HttpResponse::MethodNotAllowed() }));
    /// }
    /// ```
    pub fn route(self, path: &str, mut route: Route<St, Out>) -> Self {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    #[must_use]
    /// Register http service.
    ///
    /// Http service is any type that implements `WebServiceFactory` trait.
    ///
    /// ntex provides several services implementations:
    ///
    /// * `Resource` is an entry in resource table which corresponds to requested URL.
    /// * `Scope` is a set of resources with common root path.
    /// * `StaticFiles` is a service for static files support
    pub fn service<S>(mut self, factory: S) -> Self
    where
        S: WebServiceFactory<St, Out> + 'static,
    {
        self.services
            .push(Box::new(ServiceFactoryWrapper::new(factory)));
        self
    }

    #[must_use]
    /// Default service to be used if no matching resource could be found.
    ///
    /// It is possible to use services like `Resource`, `Route`.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .service(
    ///             web::resource("/index.html").route(web::get().to(index)))
    ///         .default_service(
    ///             web::route().to(async || { HttpResponse::NotFound() }));
    /// }
    /// ```
    ///
    /// It is also possible to use static files as default service.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// fn main() {
    ///     let app = App::default()
    ///         .service(
    ///             web::resource("/index.html").to(async || { HttpResponse::Ok() }))
    ///         .default_service(
    ///             web::to(async || { HttpResponse::NotFound() })
    ///         );
    /// }
    /// ```
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
    St: AppState,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<AppRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<()>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    /// Construct service factory, suitable for `http::HttpService`.
    ///
    /// ```rust,no_run
    /// use ntex::{web, http, server, SharedCfg};
    ///
    /// #[ntex::main]
    /// async fn main() -> std::io::Result<()> {
    ///     server::build().bind("http", "127.0.0.1:0", SharedCfg::default(), async |_|
    ///         http::HttpService::new(
    ///             web::App::default()
    ///                 .route("/index.html", web::get().to(async || { "hello_world" }))
    ///         )
    ///     )?
    ///     .run()
    ///     .await
    /// }
    /// ```
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

impl<St, In, Out, M, F> IntoServiceFactory<AppFactory<St, In, Out, M, F>, St, Request>
    for AppServices<St, In, Out, M, F>
where
    St: AppState,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
    M: Middleware<AppRouter<St, In, Out, F::Service>, St> + 'static,
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
        let cfg = WebAppConfig::new().set_state(10usize).into();

        let srv = init_service(
            App::new()
                .config(cfg)
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
