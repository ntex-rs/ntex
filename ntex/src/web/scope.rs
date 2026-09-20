use std::{cell::RefCell, marker::PhantomData, rc::Rc};

use crate::error::{Failure, IntoFailure};
use crate::http::Response;
use crate::router::{IntoPattern, ResourceDef, Router};
use crate::service::{Identity, ServiceChainFactory, boxed};
use crate::{IntoServiceFactory, Middleware, Service, ServiceFactory, factory};

use super::app_service::WebServiceRouter;
use super::dev::{WebServiceConfig, WebServiceFactory};
use super::error::{WebError, WebResponseError};
use super::guard::Guard;
use super::rmap::ResourceMap;
use super::service::{AppServiceFactory, ServiceFactoryWrapper};
use super::stack::{Filter, WebStack};
use super::{HttpService, Resource, Route, ServiceConfig, State, WebRequest, WebResponse};

type Guards = Vec<Box<dyn Guard>>;

/// Resources scope.
///
/// Scope is a set of resources with common root path.
/// Scopes collect multiple paths under a common path prefix.
/// Scope path can contain variable path segments as resources.
/// Scope prefix is always complete path segment, i.e `/app` would
/// be converted to a `/app/` and it would not match `/app` path.
///
/// You can get variable path segments from `HttpRequest::match_info()`.
/// `Path` extractor also is able to extract scope level variable segments.
///
/// ```rust
/// use ntex::web::{self, App, HttpResponse};
///
/// fn main() {
///     let app = App::default().service(
///         web::scope("/{project_id}/")
///             .service(web::resource("/path1").to(async || { HttpResponse::Ok() }))
///             .service(web::resource("/path2").route(web::get().to(async || { HttpResponse::Ok() })))
///             .service(web::resource("/path3").route(web::head().to(async || { HttpResponse::MethodNotAllowed() })))
///     );
/// }
/// ```
///
/// In the above example three routes get registered:
///  * `/{project_id}/path1` - reponds to all http method
///  * `/{project_id}/path2` - `GET` requests
///  * `/{project_id}/path3` - `HEAD` requests
///
#[derive(derive_more::Debug)]
#[debug("Scope({rdef:?})")]
pub struct Scope<St: State, In, Out = In, M = Identity, F = Filter<St, In>> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    rdef: Vec<String>,
    guards: Vec<Box<dyn Guard>>,
    external: Vec<ResourceDef>,
    case_insensitive: bool,
    ph: PhantomData<Out>,
}

/// Resources scope.
///
/// Scope is a set of resources with common root path.
#[derive(derive_more::Debug)]
#[debug("ScopeServices({rdef:?})")]
pub struct ScopeServices<St: State, In, Out, M, F> {
    middleware: M,
    rdef: Vec<String>,
    guards: Vec<Box<dyn Guard>>,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    services: Vec<Box<dyn AppServiceFactory<St, Out>>>,
    default: Option<HttpService<St, Out>>,
    external: Vec<ResourceDef>,
    case_insensitive: bool,
}

impl<St: State, In> Scope<St, In, In> {
    #[allow(clippy::needless_pass_by_value)]
    /// Create a new scope
    pub fn new<T: IntoPattern>(path: T) -> Self {
        Scope {
            middleware: Identity,
            filter: factory(Filter::new()),
            rdef: path.patterns(),
            guards: Vec::new(),
            external: Vec::new(),
            case_insensitive: false,
            ph: PhantomData,
        }
    }
}

impl<St, In, Out, M, F> Scope<St, In, Out, M, F>
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
    /// Add a match guard to this scope.
    ///
    /// The scope is selected only when its path prefix and all registered
    /// guards match. If a guard rejects the request, the application router can
    /// try another matching scope or resource; otherwise the application's
    /// default service is used.
    ///
    /// The guard applies to every resource nested in the scope.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App};
    ///
    /// App::default().service(
    ///     web::scope("/api")
    ///         .guard(guard::Header("x-api-version", "2"))
    ///         .route("/users", web::get().to(async || "Version 2 users"))
    /// );
    /// ```
    #[must_use]
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.push(Box::new(guard));
        self
    }

    /// Use ascii case-insensitive routing.
    ///
    /// Only static segments could be case-insensitive.
    #[must_use]
    pub fn case_insensitive_routing(mut self) -> Self {
        self.case_insensitive = true;
        self
    }

    /// Run external configuration as part of the scope building
    /// process
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
    ///         .service(
    ///             web::scope("/api")
    ///                 .configure(config)
    ///         )
    ///         .route("/index.html", web::get().to(async || { HttpResponse::Ok() }));
    /// }
    /// ```
    #[must_use]
    pub fn configure(
        self,
        f: impl FnOnce(&mut ServiceConfig<St, Out>),
    ) -> ScopeServices<St, In, Out, M, F> {
        let mut cfg = ServiceConfig::new(self.external);
        f(&mut cfg);

        ScopeServices {
            rdef: self.rdef,
            guards: self.guards,
            filter: self.filter,
            middleware: self.middleware,
            default: None,
            services: cfg.services,
            external: cfg.external,
            case_insensitive: self.case_insensitive,
        }
    }

    /// Register http service.
    ///
    /// This is similar to `App's` service registration.
    ///
    /// ntex web provides several services implementations:
    ///
    /// * *`Resource`* is an entry in resource table which corresponds to requested URL.
    /// * *`Scope`* is a set of resources with common root path.
    /// * *`StaticFiles`* is a service for static files support
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest};
    ///
    /// async fn index(req: HttpRequest) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default().service(
    ///         web::scope("/app").service(
    ///             web::scope("/v1")
    ///                 .service(web::resource("/test1").to(index)))
    ///     );
    /// }
    /// ```
    #[must_use]
    pub fn service(
        self,
        factory: impl WebServiceFactory<St, Out>,
    ) -> ScopeServices<St, In, Out, M, F> {
        ScopeServices {
            rdef: self.rdef,
            guards: self.guards,
            filter: self.filter,
            middleware: self.middleware,
            default: None,
            external: self.external,
            case_insensitive: self.case_insensitive,
            services: vec![Box::new(ServiceFactoryWrapper::new(factory))],
        }
    }

    /// Register a route for a path relative to this scope.
    ///
    /// This is shorthand for creating a [`Resource`] with one route and
    /// registering it with [`Scope::service()`]. The route's method and custom
    /// guards are promoted to resource guards.
    ///
    /// If those guards reject a request, the generated resource does not match
    /// and the scope router continues searching. If nothing else in the scope
    /// matches, the scope default service is used. Register an explicit
    /// [`Resource`] when route mismatches should use a resource-level fallback.
    ///
    /// Each call creates a separate resource, so the same relative path can be
    /// registered more than once with different guards.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default().service(
    ///     web::scope("/api")
    ///         .route("/items", web::get().to(async || "list"))
    ///         .route(
    ///             "/items",
    ///             web::post().to(async || HttpResponse::Created()),
    ///         )
    /// );
    /// ```
    #[must_use]
    pub fn route(self, path: &str, mut route: Route<St, Out>) -> ScopeServices<St, In, Out, M, F> {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Set the fallback service for unmatched requests within this scope.
    ///
    /// The fallback is called after the scope prefix and guards match but no
    /// nested resource matches. Without a custom fallback, the scope returns
    /// `404 Not Found`; it does not delegate to the application's fallback.
    /// Routing failures inside a matched resource are handled by that
    /// resource's fallback instead.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse};
    ///
    /// async fn not_found(req: HttpRequest) -> HttpResponse {
    ///     HttpResponse::NotFound()
    ///         .body(format!("No API resource for {}", req.path()))
    /// }
    ///
    /// App::default().service(
    ///     web::scope("/api")
    ///         .route("/health", web::get().to(async || "ready"))
    ///         .default_service(web::to(not_found))
    /// );
    /// ```
    #[must_use]
    pub fn default_service<Sf>(
        self,
        f: impl IntoServiceFactory<Sf, St, WebRequest<Out>>,
    ) -> ScopeServices<St, In, Out, M, F>
    where
        Sf: ServiceFactory<St, WebRequest<Out>, Res = WebResponse> + 'static,
        Sf::Error: WebResponseError<St, St::Error>,
        Sf::InitError: IntoFailure,
    {
        // create and configure default resource
        let default = boxed::factory(
            f.into_factory()
                .map_err(WebError::from_err)
                .map_init_err(IntoFailure::fail),
        );

        ScopeServices {
            rdef: self.rdef,
            guards: self.guards,
            filter: self.filter,
            middleware: self.middleware,
            default: Some(default),
            external: self.external,
            case_insensitive: self.case_insensitive,
            services: Vec::new(),
        }
    }

    /// Registers a request filter for this scope.
    ///
    /// The filter runs after the scope's path and guards match, but before its
    /// nested router selects a resource. It is not called for requests that do
    /// not match this scope.
    ///
    /// ```rust
    /// use std::convert::Infallible;
    /// use ntex::web::{self, App, WebRequest};
    ///
    /// async fn user(_state: &(), user_id: usize) -> String {
    ///     format!("User {user_id}")
    /// }
    ///
    /// App::new().service(
    ///     web::scope("/api")
    ///         .filter(async |req: WebRequest<()>| {
    ///             Ok::<_, Infallible>(req.map_state(|()| 42usize))
    ///         })
    ///         .route("/user", web::get().to_with_state(user)),
    /// );
    /// ```
    #[must_use]
    pub fn filter<U, R>(
        self,
        filter: impl IntoServiceFactory<U, St, WebRequest<Out>>,
    ) -> Scope<
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
        U: ServiceFactory<St, WebRequest<Out>, Res = WebRequest<R>>,
        U::Error: WebResponseError<St, St::Error>,
        U::InitError: IntoFailure,
    {
        Scope {
            filter: self.filter.and_then(
                filter
                    .into_factory()
                    .map_err(WebError::from_err)
                    .map_init_err(IntoFailure::fail),
            ),
            middleware: self.middleware,
            rdef: self.rdef,
            guards: self.guards,
            external: self.external,
            case_insensitive: self.case_insensitive,
            ph: PhantomData,
        }
    }

    /// Adds middleware around this scope.
    ///
    /// The middleware runs only after the scope's path and guards match. It
    /// wraps everything inside the scope, including its filter, nested routes,
    /// and fallback service. This means it can inspect or modify both the
    /// request and response, even when no nested route matches.
    ///
    /// ```rust
    /// use ntex::web::{self, middleware, App};
    ///
    /// App::default().service(
    ///     web::scope("/api")
    ///         .middleware(
    ///             middleware::DefaultHeaders::new()
    ///                 .header("x-api", "v1"),
    ///         )
    ///         .route("/health", web::get().to(async || "OK")),
    /// );
    /// ```
    #[must_use]
    pub fn middleware<U>(self, mw: U) -> Scope<St, In, Out, WebStack<St, U, M>, F> {
        Scope {
            middleware: WebStack::new(mw, self.middleware),
            filter: self.filter,
            rdef: self.rdef,
            guards: self.guards,
            external: self.external,
            case_insensitive: self.case_insensitive,
            ph: PhantomData,
        }
    }
}

impl<St, In, Out, M, F> ScopeServices<St, In, Out, M, F>
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
    /// Register http service.
    ///
    /// This is similar to `App's` service registration.
    ///
    /// ntex web provides several services implementations:
    ///
    /// * *`Resource`* is an entry in resource table which corresponds to requested URL.
    /// * *`Scope`* is a set of resources with common root path.
    /// * *`StaticFiles`* is a service for static files support
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest};
    ///
    /// async fn index(req: HttpRequest) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::default().service(
    ///         web::scope("/app").service(
    ///             web::scope("/v1")
    ///                 .service(web::resource("/test1").to(index)))
    ///     );
    /// }
    /// ```
    #[must_use]
    pub fn service(mut self, factory: impl WebServiceFactory<St, Out>) -> Self {
        self.services
            .push(Box::new(ServiceFactoryWrapper::new(factory)));
        self
    }

    /// Register a route for a path relative to this scope.
    ///
    /// This is shorthand for creating a [`Resource`] with one route and
    /// registering it with [`ScopeServices::service()`]. The route's method and
    /// custom guards are promoted to resource guards.
    ///
    /// Each call creates a separate resource, so the same relative path can be
    /// registered more than once with different guards.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default().service(
    ///     web::scope("/api")
    ///         .route("/items", web::get().to(async || "list"))
    ///         .route(
    ///             "/items",
    ///             web::post().to(async || HttpResponse::Created()),
    ///         )
    /// );
    /// ```
    #[must_use]
    pub fn route(self, path: &str, mut route: Route<St, Out>) -> Self {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Set the fallback service for unmatched requests within this scope.
    ///
    /// The fallback is called after the scope prefix and guards match but no
    /// nested resource matches. Without a custom fallback, the scope returns
    /// `404 Not Found`; it does not delegate to the application's fallback.
    /// Routing failures inside a matched resource are handled by that
    /// resource's fallback instead.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse};
    ///
    /// async fn not_found(req: HttpRequest) -> HttpResponse {
    ///     HttpResponse::NotFound()
    ///         .body(format!("No API resource for {}", req.path()))
    /// }
    ///
    /// App::default().service(
    ///     web::scope("/api")
    ///         .route("/health", web::get().to(async || "ready"))
    ///         .default_service(web::to(not_found))
    /// );
    /// ```
    #[must_use]
    pub fn default_service<Sf>(
        mut self,
        f: impl IntoServiceFactory<Sf, St, WebRequest<Out>>,
    ) -> Self
    where
        Sf: ServiceFactory<St, WebRequest<Out>, Res = WebResponse> + 'static,
        Sf::Error: WebResponseError<St, St::Error>,
        Sf::InitError: IntoFailure,
    {
        // create and configure default resource
        self.default = Some(boxed::factory(
            f.into_factory()
                .map_err(WebError::from_err)
                .map_init_err(IntoFailure::fail),
        ));

        self
    }
}

impl<St, Outer, In, Out, M, F> WebServiceFactory<St, Outer> for ScopeServices<St, In, Out, M, F>
where
    St: State,
    Outer: 'static,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        > + 'static,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<Outer>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    fn register(mut self, config: &mut WebServiceConfig<St, Outer>) {
        // Default service
        let default = self.default.unwrap_or_else(|| {
            boxed::factory(
                factory(async move |req: WebRequest<Out>| {
                    Ok(req.into_response(Response::NotFound().build()))
                })
                .map_init_err(|_| unreachable!()),
            )
        });

        // Web app config
        let mut cfg = WebServiceConfig::new();

        // register nested services
        for mut svc in self.services {
            svc.register(&mut cfg);
        }

        // ResourceMap tree
        let slash = self.rdef.iter().any(|s| s.ends_with('/'));
        let mut rmap = ResourceMap::new(ResourceDef::root_prefix(self.rdef.clone()));

        for mut rdef in std::mem::take(&mut self.external) {
            rmap.add(&mut rdef, None);
        }

        // Complete scope pipeline creation
        let services: Vec<_> = cfg
            .into_services()
            .into_iter()
            .map(|(rdef, srv, guards, nested)| {
                // case for scope prefix ends with '/' and
                // resource is empty pattern
                let mut rdef = if slash && rdef.pattern() == "" {
                    ResourceDef::new("/")
                } else {
                    rdef
                };
                rmap.add(&mut rdef, nested);
                (rdef, srv, RefCell::new(guards))
            })
            .collect();

        // Create router
        let mut router = Router::builder();
        if self.case_insensitive {
            router.case_insensitive();
        }
        for (path, factory, guards) in services {
            router.rdef(path.clone(), factory).2 = guards.borrow_mut().take();
        }

        // register final service
        config.register_service(
            ResourceDef::root_prefix(self.rdef),
            if self.guards.is_empty() {
                None
            } else {
                Some(self.guards)
            },
            Some(Rc::new(rmap)),
            ScopeServiceFactory {
                default,
                middleware: self.middleware,
                filter: self.filter,
                router: Rc::new(router.build()),
            },
        );
    }
}

/// Scope service
struct ScopeServiceFactory<St: State, In, Out, M, F> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    router: Rc<Router<HttpService<St, Out>, Guards>>,
    default: HttpService<St, Out>,
}

impl<St, Outer, In, Out, M, F> ServiceFactory<St, WebRequest<Outer>>
    for ScopeServiceFactory<St, In, Out, M, F>
where
    St: State,
    Outer: 'static,
    In: 'static,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        > + 'static,
    M: Middleware<WebServiceRouter<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<Outer>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    type Service = M::Service;
    type InitError = Failure;

    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        let filter = self.filter.create(st).await?;

        // router service
        Ok(self.middleware.create(
            st,
            WebServiceRouter::new(filter, self.router.clone(), self.default.clone()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use crate::http::body::{Body, ResponseBody};
    use crate::http::header::{CONTENT_TYPE, HeaderValue};
    use crate::http::{Method, StatusCode};
    use crate::util::Bytes;
    use crate::web::middleware::DefaultHeaders;
    use crate::web::test::{TestRequest, call_service, init_service, read_body};
    use crate::web::{self, App, HttpRequest, HttpResponse, WebRequest, guard};

    #[crate::rt_test]
    async fn test_scope() {
        let srv = init_service(
            App::new()
                .service(
                    web::scope("/app")
                        .service(web::resource("/path1").to(async || HttpResponse::Ok())),
                )
                .service(
                    web::scope("/app2")
                        .case_insensitive_routing()
                        .service(web::resource("/path1").to(async || HttpResponse::Ok())),
                ),
        )
        .await;

        let req = TestRequest::with_uri("/app/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app/path10").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let req = TestRequest::with_uri("/app2/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app2/Path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_scope_root() {
        let srv = init_service(
            App::new().service(
                web::scope("/app")
                    .service(web::resource("").to(async || HttpResponse::Ok()))
                    .service(web::resource("/").to(async || HttpResponse::Created())),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[crate::rt_test]
    async fn test_scope_root_multi() {
        let srv = init_service(
            App::new().service(
                web::scope(["/app", "/app2"])
                    .service(web::resource("").to(async || HttpResponse::Ok()))
                    .service(web::resource("/").to(async || HttpResponse::Created())),
            ),
        )
        .await;

        for url in &["/app", "/app2"] {
            let req = TestRequest::with_uri(url).to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        for url in &["/app/", "/app2/"] {
            let req = TestRequest::with_uri(url).to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }
    }

    #[crate::rt_test]
    async fn test_scope_root2() {
        let srv = init_service(App::new().service(
            web::scope("/app/").service(web::resource("").to(async || HttpResponse::Ok())),
        ))
        .await;

        let req = TestRequest::with_uri("/app").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let req = TestRequest::with_uri("/app/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_scope_root2_multi() {
        let srv = init_service(
            App::new().service(
                web::scope(["/app/", "/app2/"])
                    .service(web::resource("").to(async || HttpResponse::Ok())),
            ),
        )
        .await;

        for url in &["/app", "/app2"] {
            let req = TestRequest::with_uri(url).to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        }

        for url in &["/app/", "/app2/"] {
            let req = TestRequest::with_uri(url).to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    #[crate::rt_test]
    async fn test_scope_root3() {
        let srv = init_service(App::new().service(
            web::scope("/app/").service(web::resource("/").to(async || HttpResponse::Ok())),
        ))
        .await;

        let req = TestRequest::with_uri("/app").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let req = TestRequest::with_uri("/app/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_scope_route() {
        let srv = init_service(
            App::new().service(
                web::scope("app")
                    .route("/path1", web::get().to(async || HttpResponse::Ok()))
                    .route("/path1", web::delete().to(async || HttpResponse::Ok())),
            ),
        )
        .await;

        for (m, status) in &[
            (Method::GET, StatusCode::OK),
            (Method::DELETE, StatusCode::OK),
            (Method::POST, StatusCode::NOT_FOUND),
        ] {
            let req = TestRequest::with_uri("/app/path1")
                .method(m.clone())
                .to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), status.clone());
        }
    }

    #[crate::rt_test]
    async fn test_scope_route_multi() {
        let srv = init_service(
            App::new().service(
                web::scope(["app", "app2"])
                    .route("/path1", web::get().to(async || HttpResponse::Ok()))
                    .route("/path1", web::delete().to(async || HttpResponse::Ok())),
            ),
        )
        .await;

        for (m, status) in &[
            (Method::GET, StatusCode::OK),
            (Method::DELETE, StatusCode::OK),
            (Method::POST, StatusCode::NOT_FOUND),
        ] {
            let req = TestRequest::with_uri("/app/path1")
                .method(m.clone())
                .to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), status.clone());

            let req = TestRequest::with_uri("/app2/path1")
                .method(m.clone())
                .to_request();
            let resp = srv.call(req).await.unwrap();
            assert_eq!(resp.status(), status.clone());
        }
    }

    #[crate::rt_test]
    async fn test_scope_route_without_leading_slash() {
        let srv = init_service(
            App::new().service(
                web::scope("app").service(
                    web::resource("path1")
                        .route(web::get().to(async || HttpResponse::Ok()))
                        .route(web::delete().to(async || HttpResponse::Ok())),
                ),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app/path1")
            .method(Method::DELETE)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app/path1")
            .method(Method::POST)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[crate::rt_test]
    async fn test_scope_guard() {
        let srv = init_service(
            App::new()
                .service(
                    web::scope("/app")
                        .guard(guard::Get())
                        .service(web::resource("/path1").to(async || HttpResponse::Ok())),
                )
                .service(
                    web::scope("/app")
                        .guard(guard::Post())
                        .service(web::resource("/path1").to(async || HttpResponse::NotModified())),
                )
                .service(web::resource("/app/path1").to(async || HttpResponse::NoContent())),
        )
        .await;

        let req = TestRequest::with_uri("/app/path1")
            .method(Method::POST)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

        let req = TestRequest::with_uri("/app/path1")
            .method(Method::GET)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app/path1")
            .method(Method::DELETE)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[crate::rt_test]
    async fn test_scope_variable_segment() {
        let srv = init_service(App::new().service(web::scope("/ab-{project}").service(
            web::resource("/path1").to(async move |r: HttpRequest| {
                HttpResponse::Ok().body(format!("project: {}", &r.match_info()["project"]))
            }),
        )))
        .await;

        let req = TestRequest::with_uri("/ab-project1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        if let ResponseBody::Body(Body::Bytes(b)) = resp.body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: project1"));
        }

        let req = TestRequest::with_uri("/aa-project1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[crate::rt_test]
    async fn test_scope_variable_segment2() {
        let srv = init_service(App::new().service(web::scope("/ab-{project}").service(
            web::resource(["", "/"]).to(async move |r: HttpRequest| {
                HttpResponse::Ok().body(format!("project: {}", &r.match_info()["project"]))
            }),
        )))
        .await;

        let req = TestRequest::with_uri("/ab-project1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        if let ResponseBody::Body(Body::Bytes(b)) = resp.body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: project1"));
        }

        let req = TestRequest::with_uri("/ab-project1/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        if let ResponseBody::Body(Body::Bytes(b)) = resp.body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: project1"));
        }

        let req = TestRequest::with_uri("/aa-project1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[crate::rt_test]
    async fn test_nested_scope() {
        let srv = init_service(App::new().service(web::scope("/app").service(
            web::scope("/t1").service(web::resource("/path1").to(async || HttpResponse::Created())),
        )))
        .await;

        let req = TestRequest::with_uri("/app/t1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[crate::rt_test]
    async fn test_nested_scope_no_slash() {
        let srv = init_service(App::new().service(web::scope("/app").service(
            web::scope("t1").service(web::resource("/path1").to(async || HttpResponse::Created())),
        )))
        .await;

        let req = TestRequest::with_uri("/app/t1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[crate::rt_test]
    async fn test_nested_scope_root() {
        let srv = init_service(
            App::new().service(
                web::scope("/app").service(
                    web::scope("/t1")
                        .service(web::resource("").to(async || HttpResponse::Ok()))
                        .service(web::resource("/").to(async || HttpResponse::Created())),
                ),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app/t1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app/t1/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[crate::rt_test]
    async fn test_nested_scope_filter() {
        let srv = init_service(
            App::new().service(
                web::scope("/app").service(
                    web::scope("/t1")
                        .guard(guard::Get())
                        .service(web::resource("/path1").to(async || HttpResponse::Ok())),
                ),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app/t1/path1")
            .method(Method::POST)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let req = TestRequest::with_uri("/app/t1/path1")
            .method(Method::GET)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_nested_scope_with_variable_segment() {
        let srv = init_service(App::new().service(web::scope("/app").service(
            web::scope("/{project_id}").service(web::resource("/path1").to(
                async move |r: HttpRequest| {
                    HttpResponse::Created()
                        .body(format!("project: {}", &r.match_info()["project_id"]))
                },
            )),
        )))
        .await;

        let req = TestRequest::with_uri("/app/project_1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        if let ResponseBody::Body(Body::Bytes(b)) = resp.body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: project_1"));
        }
    }

    #[crate::rt_test]
    async fn test_nested2_scope_with_variable_segment() {
        let srv = init_service(App::new().service(web::scope("/app").service(
            web::scope("/{project}").service(web::scope("/{id}").service(
                web::resource("/path1").to(async move |r: HttpRequest| {
                    HttpResponse::Created().body(format!(
                        "project: {} - {}",
                        &r.match_info()["project"],
                        &r.match_info()["id"],
                    ))
                }),
            )),
        )))
        .await;

        let req = TestRequest::with_uri("/app/test/1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        if let ResponseBody::Body(Body::Bytes(b)) = resp.body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: test - 1"));
        }

        let req = TestRequest::with_uri("/app/test/1/path2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[crate::rt_test]
    async fn test_default_resource() {
        let srv = init_service(
            App::new().service(
                web::scope("/app")
                    .service(web::resource("/path1").to(async || HttpResponse::Ok()))
                    .default_service(async move |r: WebRequest<()>| {
                        Ok::<_, Infallible>(r.into_response(HttpResponse::BadRequest()))
                    }),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app/path2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let req = TestRequest::with_uri("/path2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[crate::rt_test]
    async fn test_filter() {
        let filter = std::rc::Rc::new(std::cell::Cell::new(false));
        let filter2 = filter.clone();
        let srv = init_service(
            App::new().service(
                web::scope("app")
                    .filter(async move |req: WebRequest<()>| {
                        filter2.set(true);
                        Ok::<_, Infallible>(req)
                    })
                    .route("/test", web::get().to(async || HttpResponse::Ok())),
            ),
        )
        .await;
        let req = TestRequest::with_uri("/app/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(filter.get());
    }

    #[crate::rt_test]
    async fn test_middleware() {
        let srv = init_service(
            App::new().service(
                web::scope("app")
                    .middleware(
                        DefaultHeaders::new()
                            .header(CONTENT_TYPE, HeaderValue::from_static("0001")),
                    )
                    .service(
                        web::resource("/test").route(web::get().to(async || HttpResponse::Ok())),
                    ),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[crate::rt_test]
    async fn test_scope_config_2() {
        let srv = init_service(App::new().service(web::scope("/app").configure(|s| {
            s.service(web::scope("/v1").configure(|s| {
                s.route("/", web::get().to(async || HttpResponse::Ok()));
            }));
        })))
        .await;

        let req = TestRequest::with_uri("/app/v1/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[cfg(feature = "url")]
    #[crate::rt_test]
    async fn test_url_for_external() {
        let srv = init_service(App::new().service(web::scope("/app").configure(|s| {
            s.service(web::scope("/v1").configure(|s| {
                s.external_resource("youtube", "https://youtube.com/watch/{video_id}");
                s.route(
                    "/",
                    web::get().to(async move |req: HttpRequest| {
                        HttpResponse::Ok().body(
                            req.url_for("youtube", ["xxxxxx"])
                                .unwrap()
                                .as_str()
                                .to_string(),
                        )
                    }),
                );
            }));
        })))
        .await;

        let req = TestRequest::with_uri("/app/v1/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        assert_eq!(body, &b"https://youtube.com/watch/xxxxxx"[..]);
    }

    #[cfg(feature = "url")]
    #[crate::rt_test]
    async fn test_url_for_nested() {
        let srv = init_service(App::new().service(web::scope("/a").service(
            web::scope("/b").service(web::resource("/c/{stuff}").name("c").route(web::get().to(
                async move |req: HttpRequest| {
                    HttpResponse::Ok().body(format!("{}", req.url_for("c", ["12345"]).unwrap()))
                },
            ))),
        )))
        .await;

        let req = TestRequest::with_uri("/a/b/c/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        assert_eq!(
            body,
            Bytes::from_static(b"http://localhost:8080/a/b/c/12345")
        );
    }
}
