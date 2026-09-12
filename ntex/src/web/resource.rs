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
use super::{AppState, FromRequest, Handler, HttpHandler, HttpService, WebRequest, WebResponse};

/// *Resource* is an entry in resources table which corresponds to requested URL.
///
/// Resource in turn has at least one route.
/// Route consists of an handlers objects and list of guards
/// (objects that implement `Guard` trait).
/// Resources and routes uses builder-like pattern for configuration.
/// During request handling, resource object iterate through all routes
/// and check guards for specific route, if request matches all
/// guards, route considered matched and route handler get called.
///
/// ```rust
/// use ntex::web::{self, App, HttpResponse};
///
/// fn main() {
///     let app = App::new().service(
///         web::resource("/")
///             .route(web::get().to(async || { HttpResponse::Ok() })));
/// }
/// ```
///
/// If no matching route could be found, *405* response code get returned.
/// Default behavior could be overriden with `default_resource()` method.
#[derive(derive_more::Debug)]
#[debug("Resource({rdef:?})")]
pub struct Resource<St: AppState, In, Out = In, M = Identity, F = Filter<St, In>> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    rdef: Vec<String>,
    name: Option<String>,
    guards: Vec<Box<dyn Guard>>,
    ph: PhantomData<Out>,
}

#[derive(derive_more::Debug)]
#[debug("Resource({rdef:?})")]
pub struct ResourceServices<St: AppState, In, Out, M, F> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest<In>>,
    rdef: Vec<String>,
    name: Option<String>,
    guards: Vec<Box<dyn Guard>>,
    routes: Vec<Route<St, Out>>,
    default: Option<HttpService<St, Out>>,
}

impl<St: AppState, In: 'static> Resource<St, In, In> {
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
    /// Set resource name.
    ///
    /// Name is used for url generation.
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

    #[must_use]
    /// Add match guard to a resource.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App, HttpResponse};
    ///
    /// async fn index(data: web::types::Path<(String, String)>) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .service(
    ///             web::resource("/app")
    ///                 .guard(guard::Header("content-type", "text/plain"))
    ///                 .route(web::get().to(index))
    ///         )
    ///         .service(
    ///             web::resource("/app")
    ///                 .guard(guard::Header("content-type", "text/json"))
    ///                 .route(web::get().to(async || { HttpResponse::MethodNotAllowed() }))
    ///         );
    /// }
    /// ```
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.push(Box::new(guard));
        self
    }

    pub(crate) fn add_guards(mut self, guards: Vec<Box<dyn Guard>>) -> Self {
        self.guards.extend(guards);
        self
    }

    #[must_use]
    /// Register request filter.
    ///
    /// This is similar to `App's` filters, but filter get invoked on resource level.
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

    #[must_use]
    /// Register a resource middleware.
    ///
    /// This is similar to `App's` middlewares, but middleware get invoked on resource level.
    /// Resource level middlewares are not allowed to change response
    /// type (i.e modify response's body).
    pub fn middleware<U>(self, mw: U) -> Resource<St, In, Out, WebStack<St, M, U>, F> {
        Resource {
            middleware: WebStack::new(self.middleware, mw),
            filter: self.filter,
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            ph: PhantomData,
        }
    }

    #[must_use]
    /// Register a new route.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App, HttpResponse};
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::resource("/").route(
    ///             web::route()
    ///                 .guard(guard::Any(guard::Get()).or(guard::Put()))
    ///                 .guard(guard::Header("Content-Type", "text/plain"))
    ///                 .to(async || { HttpResponse::Ok() }))
    ///     );
    /// }
    /// ```
    ///
    /// Multiple routes could be added to a resource. Resource object uses
    /// match guards for route selection.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App};
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::resource("/container/")
    ///             .route([
    ///                 web::get().to(get_handler),
    ///                 web::post().to(post_handler),
    ///                 web::delete().to(delete_handler)
    ///             ])
    ///     );
    /// }
    /// # async fn get_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// # async fn post_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// # async fn delete_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// ```
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

    #[must_use]
    /// Register a new route and add handler.
    ///
    /// This route matches all requests.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse};
    ///
    /// async fn index(req: HttpRequest) -> HttpResponse {
    ///     unimplemented!()
    /// }
    ///
    /// App::new().service(web::resource("/").to(index));
    /// ```
    ///
    /// This is shortcut for:
    ///
    /// ```rust
    /// # use ntex::web::{self, *};
    /// # async fn index(req: HttpRequest) -> HttpResponse { unimplemented!() }
    /// App::new().service(web::resource("/").route(web::route().to(index)));
    /// ```
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

    #[must_use]
    /// Default service to be used if no matching route could be found.
    ///
    /// By default *405* response get returned. Resource does not use
    /// default handler from `App` or `Scope`.
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
    St: AppState,
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
    #[must_use]
    /// Register a new route.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App, HttpResponse};
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::resource("/").route(
    ///             web::route()
    ///                 .guard(guard::Any(guard::Get()).or(guard::Put()))
    ///                 .guard(guard::Header("Content-Type", "text/plain"))
    ///                 .to(async || { HttpResponse::Ok() }))
    ///     );
    /// }
    /// ```
    ///
    /// Multiple routes could be added to a resource. Resource object uses
    /// match guards for route selection.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App};
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::resource("/container/")
    ///             .route([
    ///                 web::get().to(get_handler),
    ///                 web::post().to(post_handler),
    ///                 web::delete().to(delete_handler)
    ///             ])
    ///     );
    /// }
    /// # async fn get_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// # async fn post_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// # async fn delete_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// ```
    pub fn route<R>(mut self, route: R) -> Self
    where
        R: IntoRoutes<St, Out>,
    {
        for route in route.routes() {
            self.routes.push(route);
        }
        self
    }

    #[must_use]
    /// Register a new route and add handler.
    ///
    /// This route matches all requests.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse};
    ///
    /// async fn index(req: HttpRequest) -> HttpResponse {
    ///     unimplemented!()
    /// }
    ///
    /// App::new().service(web::resource("/").to(index));
    /// ```
    ///
    /// This is shortcut for:
    ///
    /// ```rust
    /// # use ntex::web::{self, *};
    /// # async fn index(req: HttpRequest) -> HttpResponse { unimplemented!() }
    /// App::new().service(web::resource("/").route(web::route().to(index)));
    /// ```
    pub fn to<Args>(mut self, handler: impl Handler<St, Args>) -> Self
    where
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St, St::Error>,
    {
        self.routes.push(Route::new().to(handler));
        self
    }

    #[must_use]
    /// Default service to be used if no matching route could be found.
    ///
    /// By default *405* response get returned. Resource does not use
    /// default handler from `App` or `Scope`.
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
    St: AppState,
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
    St: AppState,
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
pub struct ResourceServiceFactory<St: AppState, In, Out, M, F> {
    middleware: M,
    filter: F,
    routes: Vec<Route<St, Out>>,
    default: Option<HttpService<St, Out>>,
    ph: PhantomData<In>,
}

impl<St, Outer, In, Out, M, F> ServiceFactory<St, WebRequest<Outer>>
    for ResourceServiceFactory<St, In, Out, M, F>
where
    St: AppState,
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
pub struct ResourceService<St: AppState, In, Out, F> {
    filter: F,
    routes: Vec<RouteService<St, Out>>,
    default: Option<HttpHandler<St, Out>>,
    ph: PhantomData<In>,
}

impl<St, In, Out, F> Service<St, WebRequest<In>> for ResourceService<St, In, Out, F>
where
    St: AppState,
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
