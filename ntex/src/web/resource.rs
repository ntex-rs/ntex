use std::marker::PhantomData;

use crate::error::{Failure, IntoFailure};
use crate::http::Response;
use crate::router::{IntoPattern, ResourceDef};
use crate::service::{Identity, ServiceChainFactory};
use crate::{Ctx, IntoServiceFactory, Middleware, Service, ServiceFactory, factory};

use super::dev::{WebServiceConfig, WebServiceFactory, insert_slash};
use super::error::{WebError, WebResponseError};
use super::guard::Guard;
use super::route::{IntoRoutes, Route, RouteService};
use super::stack::{Filter, WebStack};
use super::{
    FromRequest, Handler, HandlerSt, HttpHandler, HttpService, State, WebRequest, WebResponse,
};

/// Groups routes and configuration for one or more URL patterns.
///
/// A resource is useful when several routes belong to the same path. Along
/// with those routes, it can have its own guards, filters, middleware, name,
/// and fallback service. Create one with [`web::resource()`], then register it
/// with [`App::service()`] or [`Scope::service()`].
///
/// For each request, the router first checks the resource's path and guards.
/// If they do not match, it keeps looking for another resource or scope. Once
/// this resource is selected, its middleware and filters run, followed by its
/// routes in registration order. The first route whose guards all pass handles
/// the request.
///
/// When none of the routes match, the resource uses its own fallback. By
/// default, that fallback returns `405 Method Not Allowed`.
///
/// Use [`Resource::route()`] to add guarded routes, or [`Resource::to()`] to
/// add an unguarded handler that accepts any request reaching the resource.
///
/// ```rust
/// use ntex::web::{self, App, HttpResponse};
///
/// App::default().service(
///     web::resource("/users")
///         .route(web::get().to(async || "users"))
///         .route(web::post().to(async || HttpResponse::Created()))
///         .default_service(web::to(async || {
///             HttpResponse::MethodNotAllowed().body("Use GET or POST")
///         })),
/// );
/// ```
///
/// [`App::service()`]: super::App::service
/// [`Scope::service()`]: super::Scope::service
/// [`web::resource()`]: super::resource
#[derive(derive_more::Debug)]
#[debug("Resource({rdef:?})")]
pub struct Resource<St: State, In, Out = In, M = Identity, F = Filter<St, In>> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    rdef: Vec<String>,
    name: Option<String>,
    guards: Vec<Box<dyn Guard>>,
    ph: PhantomData<Out>,
}

#[derive(derive_more::Debug)]
#[debug("Resource({rdef:?})")]
pub struct ResourceServices<St: State, In, Out, M, F> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    rdef: Vec<String>,
    name: Option<String>,
    guards: Vec<Box<dyn Guard>>,
    routes: Vec<Route<St, Out>>,
    default: Option<HttpService<St, Out>>,
}

impl<St: State, In: 'static> Resource<St, In, In> {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new<T: IntoPattern>(path: T) -> Resource<St, In, In> {
        Resource {
            rdef: path.patterns(),
            name: None,
            middleware: Identity,
            filter: factory(Filter::new()),
            guards: Vec::new(),
            ph: PhantomData,
        }
    }
}

impl<St, In, Out, M, F> Resource<St, In, Out, M, F>
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
    /// Set resource name.
    ///
    /// Name is used for url generation.
    #[must_use]
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    /// Add a match guard to this resource.
    ///
    /// The resource is selected only when its path and all registered guards
    /// match. If a guard rejects the request, the router can try another
    /// resource with the same path; otherwise the containing scope or
    /// application fallback is used.
    ///
    /// Resource guards run before route selection. Use [`Route::guard()`] when
    /// the condition should choose between routes inside one resource.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App};
    ///
    /// App::default()
    ///     .service(
    ///         web::resource("/items")
    ///             .guard(guard::Header("accept", "application/json"))
    ///             .to(async || "JSON items")
    ///     )
    ///     .service(
    ///         web::resource("/items")
    ///             .guard(guard::Header("accept", "text/plain"))
    ///             .to(async || "Text items")
    ///     );
    /// ```
    #[must_use]
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.push(Box::new(guard));
        self
    }

    pub(crate) fn add_guards(mut self, guards: Vec<Box<dyn Guard>>) -> Self {
        self.guards.extend(guards);
        self
    }

    /// Registers a request filter for this resource.
    ///
    /// The filter runs after the resource's path and guards match, but before
    /// its routes are checked. It therefore also runs when no route matches
    /// and the resource's default service is used.
    ///
    /// ```rust
    /// use std::convert::Infallible;
    /// use ntex::web::{self, App, WebRequest};
    ///
    /// async fn item(_state: &(), item_id: usize) -> String {
    ///     format!("Item {item_id}")
    /// }
    ///
    /// App::new().service(
    ///     web::resource("/item")
    ///         .filter(async |req: WebRequest<()>| {
    ///             Ok::<_, Infallible>(req.map_state(|()| 10usize))
    ///         })
    ///         .route(web::get().to_with_state(item)),
    /// );
    /// ```
    #[must_use]
    pub fn filter<U, R>(
        self,
        filter: impl IntoServiceFactory<U, St, WebRequest<Out>>,
    ) -> Resource<
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
        Resource {
            filter: self.filter.and_then(
                filter
                    .into_factory()
                    .map_err(WebError::from_err)
                    .map_init_err(IntoFailure::fail),
            ),
            middleware: self.middleware,
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            ph: PhantomData,
        }
    }

    /// Registers a middleware for this resource.
    ///
    /// The middleware runs only after the resource's path and guards match. It
    /// wraps the resource filter, routes, and fallback service, so it can
    /// inspect or modify both the request and response. It also runs when no
    /// route matches and the resource fallback handles the request.
    ///
    /// ```rust
    /// use ntex::web::{self, middleware, App};
    ///
    /// App::default().service(
    ///     web::resource("/items")
    ///         .middleware(middleware::Logger::default())
    ///         .route(web::get().to(async || "Items")),
    /// );
    /// ```
    #[must_use]
    pub fn middleware<U>(self, mw: U) -> Resource<St, In, Out, WebStack<St, U, M>, F> {
        Resource {
            middleware: WebStack::new(mw, self.middleware),
            filter: self.filter,
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            ph: PhantomData,
        }
    }

    /// Add one or more routes to this resource.
    ///
    /// Routes are checked in registration order after the resource path and
    /// resource guards match. The first route whose method and custom guards
    /// accept the request is called. If no route matches, the resource's
    /// default service is used; without a custom default, it returns
    /// `405 Method Not Allowed`.
    ///
    /// A single [`Route`] or a collection of routes can be supplied.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default().service(
    ///     web::resource("/items").route([
    ///         web::get().to(async || "list"),
    ///         web::post().to(async || HttpResponse::Created()),
    ///         web::delete().to(async || HttpResponse::NoContent()),
    ///     ])
    /// );
    /// ```
    #[must_use]
    pub fn route<R>(self, route: R) -> ResourceServices<St, In, Out, M, F>
    where
        R: IntoRoutes<St, Out>,
    {
        let mut routes = Vec::new();
        for route in route.routes() {
            routes.push(route);
        }

        ResourceServices {
            routes,
            name: self.name,
            rdef: self.rdef,
            guards: self.guards,
            filter: self.filter,
            middleware: self.middleware,
            default: None,
        }
    }

    /// Register route with a handler.
    ///
    /// The route matches every request after this resource's path and guards
    /// match. The handler receives request extractor values and returns a type
    /// implementing [`Responder`](super::Responder).
    ///
    /// ```rust
    /// use ntex::web::{self, App};
    ///
    /// async fn show_user(id: web::types::Path<u32>) -> String {
    ///     format!("User {}", id.into_inner())
    /// }
    ///
    /// App::default()
    ///     .service(web::resource("/users/{id}").to(show_user));
    /// ```
    ///
    /// This is equivalent to `resource.route(web::route().to(handler))`.
    #[must_use]
    pub fn to<Args>(self, h: impl Handler<St, Args>) -> ResourceServices<St, In, Out, M, F>
    where
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St, St::Error>,
    {
        ResourceServices {
            name: self.name,
            rdef: self.rdef,
            guards: self.guards,
            filter: self.filter,
            middleware: self.middleware,
            default: None,
            routes: vec![Route::new().to(h)],
        }
    }

    /// Register a state-aware handler as a new route.
    ///
    /// The handler receives a shared reference to the application state,
    /// followed by the current request state and any request extractors.
    ///
    /// ```rust
    /// use ntex::web;
    ///
    /// struct AppState {
    ///     greeting: &'static str,
    /// }
    ///
    /// impl web::State for AppState {
    ///     type Error = web::DefaultError;
    /// }
    ///
    /// async fn index(
    ///     state: &AppState,
    ///     request_state: (),
    ///     name: web::types::Path<String>,
    /// ) -> String {
    ///     let _ = request_state;
    ///     format!("{}, {}!", state.greeting, name.into_inner())
    /// }
    ///
    /// web::App::<AppState>::new()
    ///     .service(web::resource("/{name}").to_with_state(index));
    /// ```
    ///
    /// This is equivalent to `resource.route(web::route().to_with_state(handler))`.
    #[must_use]
    pub fn to_with_state<Args>(
        self,
        h: impl HandlerSt<St, Out, Args>,
    ) -> ResourceServices<St, In, Out, M, F>
    where
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St, St::Error>,
    {
        ResourceServices {
            name: self.name,
            rdef: self.rdef,
            guards: self.guards,
            filter: self.filter,
            middleware: self.middleware,
            default: None,
            routes: vec![Route::new().to_with_state(h)],
        }
    }

    /// Set the fallback service for this resource.
    ///
    /// The fallback is called after the resource path and guards match but none
    /// of its routes match, commonly because the request method is unsupported.
    /// Without a custom fallback, the resource returns
    /// `405 Method Not Allowed`. It does not delegate to a scope or application
    /// fallback.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default().service(
    ///     web::resource("/items")
    ///         .route(web::get().to(async || "items"))
    ///         .default_service(web::to(async || {
    ///             HttpResponse::MethodNotAllowed().body("Use GET")
    ///         }))
    /// );
    /// ```
    #[must_use]
    pub fn default_service<S>(
        self,
        f: impl IntoServiceFactory<S, St, WebRequest<Out>>,
    ) -> ResourceServices<St, In, Out, M, F>
    where
        S: ServiceFactory<St, WebRequest<Out>, Res = WebResponse> + 'static,
        S::Error: WebResponseError<St, St::Error>,
        S::InitError: IntoFailure,
    {
        // create and configure default resource
        ResourceServices {
            name: self.name,
            rdef: self.rdef,
            guards: self.guards,
            filter: self.filter,
            routes: Vec::new(),
            middleware: self.middleware,
            default: Some(HttpService::new(
                f.into_factory()
                    .map_err(WebError::from_err)
                    .map_init_err(IntoFailure::fail),
            )),
        }
    }
}

impl<St, In, Out, M, F> ResourceServices<St, In, Out, M, F>
where
    St: State,
    In: 'static,
    Out: 'static,
    M: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        >,
{
    /// Add one or more routes to this resource.
    ///
    /// Routes are checked in registration order after the resource path and
    /// resource guards match. The first route whose method and custom guards
    /// accept the request is called. If no route matches, the resource's
    /// default service is used; without a custom default, it returns
    /// `405 Method Not Allowed`.
    ///
    /// A single [`Route`] or a collection of routes can be supplied.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default().service(
    ///     web::resource("/items")
    ///         .route(web::get().to(async || "list"))
    ///         .route([
    ///             web::post().to(async || HttpResponse::Created()),
    ///             web::delete().to(async || HttpResponse::NoContent()),
    ///         ])
    /// );
    /// ```
    #[must_use]
    pub fn route<R>(mut self, route: R) -> Self
    where
        R: IntoRoutes<St, Out>,
    {
        for route in route.routes() {
            self.routes.push(route);
        }
        self
    }

    /// Add an unguarded route with a handler.
    ///
    /// The route matches every request not accepted by an earlier route.
    /// Routes are checked in registration order, so this catch-all route should
    /// normally be added last; routes added after it cannot be selected.
    ///
    /// The handler receives request extractor values and returns a type
    /// implementing [`Responder`](super::Responder).
    ///
    /// ```rust
    /// use ntex::web::{self, App};
    ///
    /// App::default().service(
    ///     web::resource("/items")
    ///         .route(web::get().to(async || "list"))
    ///         .to(async || "Unsupported request")
    /// );
    /// ```
    #[must_use]
    pub fn to<Args>(mut self, handler: impl Handler<St, Args>) -> Self
    where
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St, St::Error>,
    {
        self.routes.push(Route::new().to(handler));
        self
    }

    /// Add a state-aware handler as a new route.
    ///
    /// The route has no guards and matches any request not accepted by an
    /// earlier route. The handler receives a shared reference to the
    /// application state, followed by the current request state and any request
    /// extractors.
    ///
    /// ```rust
    /// use ntex::web;
    ///
    /// struct AppState;
    ///
    /// impl web::State for AppState {
    ///     type Error = web::DefaultError;
    /// }
    ///
    /// async fn fallback(
    ///     _state: &AppState,
    ///     _request_state: (),
    ///     req: web::HttpRequest,
    /// ) -> String {
    ///     format!("No route for {}", req.path())
    /// }
    ///
    /// web::App::<AppState>::new().service(
    ///     web::resource("/")
    ///         .route(web::get().to(async || "GET"))
    ///         .to_with_state(fallback)
    /// );
    /// ```
    #[must_use]
    pub fn to_with_state<Args>(mut self, handler: impl HandlerSt<St, Out, Args>) -> Self
    where
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St, St::Error>,
    {
        self.routes.push(Route::new().to_with_state(handler));
        self
    }

    /// Set the fallback service for this resource.
    ///
    /// The fallback is called after the resource path and guards match but none
    /// of its routes match, commonly because the request method is unsupported.
    /// Without a custom fallback, the resource returns
    /// `405 Method Not Allowed`. It does not delegate to a scope or application
    /// fallback.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// App::default().service(
    ///     web::resource("/items")
    ///         .route(web::get().to(async || "items"))
    ///         .default_service(web::to(async || {
    ///             HttpResponse::MethodNotAllowed().body("Use GET")
    ///         }))
    /// );
    /// ```
    #[must_use]
    pub fn default_service<S>(mut self, f: impl IntoServiceFactory<S, St, WebRequest<Out>>) -> Self
    where
        S: ServiceFactory<St, WebRequest<Out>, Res = WebResponse> + 'static,
        S::Error: WebResponseError<St, St::Error>,
        S::InitError: IntoFailure,
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

impl<St, Outer, In, Out, M, F> WebServiceFactory<St, Outer> for ResourceServices<St, In, Out, M, F>
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
    M: Middleware<ResourceService<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<Outer>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    fn register(mut self, config: &mut WebServiceConfig<St, Outer>) {
        let guards = if self.guards.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.guards))
        };
        let mut rdef = if config.is_root() || !self.rdef.is_empty() {
            ResourceDef::new(insert_slash(self.rdef.clone()))
        } else {
            ResourceDef::new(self.rdef.clone())
        };
        if let Some(ref name) = self.name {
            rdef.name_mut().clone_from(name);
        }

        config.register_service(
            rdef,
            guards,
            None,
            ResourceServiceFactory {
                middleware: self.middleware,
                filter: self.filter,
                routes: self.routes,
                default: self.default.take(),
                ph: PhantomData,
            },
        );
    }
}

impl<St, Outer, In, Out, M, F>
    IntoServiceFactory<
        ResourceServiceFactory<St, In, Out, M, ServiceChainFactory<F, St, WebRequest<In>>>,
        St,
        WebRequest<Outer>,
    > for ResourceServices<St, In, Out, M, F>
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
    M: Middleware<ResourceService<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<Outer>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    fn into_factory(
        mut self,
    ) -> ResourceServiceFactory<St, In, Out, M, ServiceChainFactory<F, St, WebRequest<In>>> {
        ResourceServiceFactory {
            middleware: self.middleware,
            filter: self.filter,
            routes: self.routes,
            default: self.default.take(),
            ph: PhantomData,
        }
    }
}

/// Resource service factory
#[derive(derive_more::Debug)]
#[debug("ResourceServiceFactory")]
pub struct ResourceServiceFactory<St: State, In, Out, M, F> {
    middleware: M,
    filter: F,
    routes: Vec<Route<St, Out>>,
    default: Option<HttpService<St, Out>>,
    ph: PhantomData<In>,
}

impl<St, Outer, In, Out, M, F> ServiceFactory<St, WebRequest<Outer>>
    for ResourceServiceFactory<St, In, Out, M, F>
where
    St: State,
    Out: 'static,
    F: ServiceFactory<
            St,
            WebRequest<In>,
            Res = WebRequest<Out>,
            Error = WebError<St, St::Error>,
            InitError = Failure,
        > + 'static,
    M: Middleware<ResourceService<St, In, Out, F::Service>, St> + 'static,
    M::Service: Service<St, WebRequest<Outer>, Res = WebResponse, Error = WebError<St, St::Error>>,
{
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    type Service = M::Service;
    type InitError = Failure;

    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        let filter = self.filter.create(st).await?;
        let default = if let Some(ref default) = self.default {
            Some(default.create(st).await?)
        } else {
            None
        };

        Ok(self.middleware.create(
            st,
            ResourceService {
                filter,
                default,
                routes: self.routes.iter().map(Route::service).collect(),
                ph: PhantomData,
            },
        ))
    }
}

/// Resource service
#[derive(derive_more::Debug)]
#[debug("ResourceService")]
pub struct ResourceService<St: State, In, Out, F> {
    filter: F,
    routes: Vec<RouteService<St, Out>>,
    default: Option<HttpHandler<St, Out>>,
    ph: PhantomData<In>,
}

impl<St, In, Out, F> Service<St, WebRequest<In>> for ResourceService<St, In, Out, F>
where
    St: State,
    F: Service<St, WebRequest<In>, Res = WebRequest<Out>, Error = WebError<St, St::Error>>,
{
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    async fn call(
        &self,
        req: WebRequest<In>,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        let mut req = ctx.call(&self.filter, req).await?;

        for route in &self.routes {
            if route.check(&mut req) {
                return ctx.call(route, req).await;
            }
        }
        if let Some(ref default) = self.default {
            ctx.call(default, req).await
        } else {
            Ok(WebResponse::new(
                Response::MethodNotAllowed().build(),
                req.into_parts().0,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use crate::http::{Method, StatusCode};
    use crate::time::{Millis, sleep};
    use crate::web::test::{TestRequest, call_service, init_service};
    use crate::web::{self, App, HttpResponse, guard, request::WebRequest};

    #[crate::rt_test]
    async fn test_filter() {
        let filter = std::rc::Rc::new(std::cell::Cell::new(false));
        let filter2 = filter.clone();
        let srv = init_service(
            App::new().service(
                web::resource("/test")
                    .filter(async move |req: WebRequest<()>| {
                        filter2.set(true);
                        Ok::<_, Infallible>(req)
                    })
                    .route(web::get().to(async || HttpResponse::Ok())),
            ),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(filter.get());
    }

    #[crate::rt_test]
    async fn test_to() {
        let srv = init_service(App::new().service(web::resource("/test").to(async || {
            sleep(Millis(100)).await;
            HttpResponse::Ok()
        })))
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_pattern() {
        let srv = init_service(
            App::new().service(web::resource(["/test", "/test2"]).to(async || HttpResponse::Ok())),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let req = TestRequest::with_uri("/test2").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_default_resource() {
        let srv = init_service(
            App::new()
                .service(web::resource("/test").route(web::get().to(async || HttpResponse::Ok())))
                .default_service(async move |r: WebRequest<()>| {
                    Ok::<_, Infallible>(r.into_response(HttpResponse::BadRequest()))
                }),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test")
            .method(Method::POST)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let srv = init_service(
            App::new().service(
                web::resource("/test")
                    .route(web::get().to(async || HttpResponse::Ok()))
                    .default_service(async move |r: WebRequest<()>| {
                        Ok::<_, Infallible>(r.into_response(HttpResponse::BadRequest()))
                    }),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test")
            .method(Method::POST)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[crate::rt_test]
    async fn test_resource_guards() {
        let srv = init_service(
            App::new()
                .service(
                    web::resource("/test/{p}")
                        .guard(guard::Get())
                        .to(async || HttpResponse::Ok()),
                )
                .service(
                    web::resource("/test/{p}")
                        .guard(guard::Put())
                        .to(async || HttpResponse::Created()),
                )
                .service(
                    web::resource("/test/{p}")
                        .guard(guard::Delete())
                        .to(async || HttpResponse::NoContent()),
                ),
        )
        .await;

        let req = TestRequest::with_uri("/test/it")
            .method(Method::GET)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test/it")
            .method(Method::PUT)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = TestRequest::with_uri("/test/it")
            .method(Method::DELETE)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
}
