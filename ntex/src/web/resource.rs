use std::fmt;

use crate::router::{IntoPattern, ResourceDef};
use crate::service::dev::{AndThen, ServiceChain, ServiceChainFactory};
use crate::service::{Ctx, cfg::SharedCfg, factory_with_st, svc};
use crate::service::{Identity, IntoServiceFactory, Middleware, Service, ServiceFactory};
use crate::{http::Response, util::Extensions};

use super::dev::{WebServiceConfig, WebServiceFactory, insert_slash};
use super::extract::FromRequest;
use super::guard::Guard;
use super::route::{IntoRoutes, Route, RouteService};
use super::stack::{Filter, WebStack};
use super::{AppState, Handler, HttpHandler, HttpService, WebRequest, WebResponse};

type ResourcePipeline<St, F> = ServiceChain<AndThen<F, ResourceRouter<St>>, St, WebRequest>;

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
pub struct Resource<St: AppState, M = Identity, F = Filter<St>> {
    middleware: M,
    filter: ServiceChainFactory<F, St, WebRequest, SharedCfg>,
    rdef: Vec<String>,
    name: Option<String>,
    routes: Vec<Route<St>>,
    state: Option<Extensions>,
    guards: Vec<Box<dyn Guard>>,
    default: Option<HttpService<St>>,
}

impl<St: AppState> Resource<St> {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new<T: IntoPattern>(path: T) -> Resource<St> {
        Resource {
            routes: Vec::new(),
            rdef: path.patterns(),
            name: None,
            state: None,
            middleware: Identity,
            filter: factory_with_st(Filter::new()),
            guards: Vec::new(),
            default: None,
        }
    }
}

impl<St, M, Sf> Resource<St, M, Sf>
where
    St: AppState,
    Sf: ServiceFactory<
            St,
            WebRequest,
            SharedCfg,
            Res = WebRequest,
            Error = St::Error,
            InitError = (),
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
    /// Provide resource specific state.
    ///
    /// This method allows to add extractor configuration or specific
    /// state available via `State<T>` extractor. Provided state is available
    /// for all routes registered for the current resource.
    /// Resource state overrides state registered by `App::state()` method.
    ///
    /// ```rust
    /// use ntex::web::{self, App, FromRequest};
    ///
    /// /// extract text data from request
    /// async fn index(body: String) -> String {
    ///     format!("Body {}!", body)
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::resource("/index.html")
    ///           // limit size of the payload
    ///           .state(web::types::PayloadConfig::new(4096))
    ///           .route(
    ///               // register handler
    ///               web::get().to(index)
    ///           ));
    /// }
    /// ```
    pub fn state<D: 'static>(mut self, st: D) -> Self {
        if self.state.is_none() {
            self.state = Some(Extensions::new());
        }
        self.state.as_mut().unwrap().insert(st);
        self
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
    pub fn route<R>(mut self, route: R) -> Self
    where
        R: IntoRoutes<St>,
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
    pub fn to<F, Args>(mut self, handler: F) -> Self
    where
        F: Handler<St, Args> + 'static,
        Args: FromRequest<St> + 'static,
        Args::Error: Into<St::Error>,
    {
        self.routes.push(Route::new().to(handler));
        self
    }

    #[must_use]
    /// Register request filter.
    ///
    /// This is similar to `App's` filters, but filter get invoked on resource level.
    pub fn filter<U>(
        self,
        filter: impl IntoServiceFactory<U, St, WebRequest, SharedCfg>,
    ) -> Resource<
        St,
        M,
        impl ServiceFactory<
            St,
            WebRequest,
            SharedCfg,
            Res = WebRequest,
            Error = St::Error,
            InitError = (),
        >,
    >
    where
        U: ServiceFactory<St, WebRequest, SharedCfg, Res = WebRequest, Error = St::Error>,
    {
        Resource {
            filter: self
                .filter
                .and_then(filter.into_factory().map_init_err(|_| ())),
            middleware: self.middleware,
            rdef: self.rdef,
            name: self.name,
            state: self.state,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
        }
    }

    #[must_use]
    /// Register a resource middleware.
    ///
    /// This is similar to `App's` middlewares, but middleware get invoked on resource level.
    /// Resource level middlewares are not allowed to change response
    /// type (i.e modify response's body).
    pub fn middleware<U>(self, mw: U) -> Resource<St, WebStack<St, M, U>, Sf> {
        Resource {
            middleware: WebStack::new(self.middleware, mw),
            filter: self.filter,
            rdef: self.rdef,
            name: self.name,
            state: self.state,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
        }
    }

    #[must_use]
    /// Default service to be used if no matching route could be found.
    ///
    /// By default *405* response get returned. Resource does not use
    /// default handler from `App` or `Scope`.
    pub fn default_service<S>(
        mut self,
        f: impl IntoServiceFactory<S, St, WebRequest, SharedCfg>,
    ) -> Self
    where
        S: ServiceFactory<St, WebRequest, SharedCfg, Res = WebResponse, Error = St::Error>
            + 'static,
        S::InitError: fmt::Debug,
    {
        // create and configure default resource
        self.default = Some(HttpService::new(f.into_factory().map_init_err(|e| {
            log::error!("Cannot construct default service: {e:?}");
        })));

        self
    }
}

impl<St, M, Sf> WebServiceFactory<St> for Resource<St, M, Sf>
where
    St: AppState,
    Sf: ServiceFactory<
            St,
            WebRequest,
            SharedCfg,
            Res = WebRequest,
            Error = St::Error,
            InitError = (),
        > + 'static,
    M: Middleware<ResourcePipeline<St, Sf::Service>, SharedCfg> + 'static,
    M::Service: Service<St, WebRequest, Res = WebResponse, Error = St::Error>,
{
    fn register(mut self, config: &mut WebServiceConfig<St>) {
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

        let router_factory = ResourceRouterFactory {
            routes: self.routes,
            default: self.default.take(),
        };

        config.register_service(
            rdef,
            ResourceServiceFactory {
                middleware: self.middleware,
                filter: self.filter,
                routing: router_factory,
            },
            guards,
            None,
        );
    }
}

impl<St, M, Sf>
    IntoServiceFactory<
        ResourceServiceFactory<St, M, ServiceChainFactory<Sf, St, WebRequest, SharedCfg>>,
        St,
        WebRequest,
        SharedCfg,
    > for Resource<St, M, Sf>
where
    St: AppState,
    Sf: ServiceFactory<
            St,
            WebRequest,
            SharedCfg,
            Res = WebRequest,
            Error = St::Error,
            InitError = (),
        > + 'static,
    M: Middleware<ResourcePipeline<St, Sf::Service>, SharedCfg> + 'static,
    M::Service: Service<St, WebRequest, Res = WebResponse, Error = St::Error>,
{
    fn into_factory(
        mut self,
    ) -> ResourceServiceFactory<St, M, ServiceChainFactory<Sf, St, WebRequest, SharedCfg>> {
        let router_factory = ResourceRouterFactory {
            routes: self.routes,
            default: self.default.take(),
        };

        ResourceServiceFactory {
            middleware: self.middleware,
            filter: self.filter,
            routing: router_factory,
        }
    }
}

/// Resource service
#[derive(derive_more::Debug)]
#[debug("ResourceServiceFactory")]
pub struct ResourceServiceFactory<St: AppState, M, F> {
    middleware: M,
    filter: F,
    routing: ResourceRouterFactory<St>,
}

impl<St, M, F> ServiceFactory<St, WebRequest, SharedCfg> for ResourceServiceFactory<St, M, F>
where
    St: AppState,
    M: Middleware<ResourcePipeline<St, F::Service>, SharedCfg> + 'static,
    M::Service: Service<St, WebRequest, Res = WebResponse, Error = St::Error>,
    F: ServiceFactory<
            St,
            WebRequest,
            SharedCfg,
            Res = WebRequest,
            Error = St::Error,
            InitError = (),
        > + 'static,
{
    type Res = WebResponse;
    type Error = St::Error;

    type Service = M::Service;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let filter = self.filter.create(cfg).await?;
        let routing = self.routing.create(cfg).await?;
        Ok(self.middleware.create(svc(filter).and_then(routing), cfg))
    }
}

struct ResourceRouterFactory<St: AppState> {
    routes: Vec<Route<St>>,
    default: Option<HttpService<St>>,
}

impl<St> ServiceFactory<St, WebRequest, SharedCfg> for ResourceRouterFactory<St>
where
    St: AppState,
{
    type Res = WebResponse;
    type Error = St::Error;

    type Service = ResourceRouter<St>;
    type InitError = ();

    async fn create(&self, cfg: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        let default = if let Some(ref default) = self.default {
            Some(default.create(cfg).await?)
        } else {
            None
        };
        Ok(ResourceRouter {
            default,
            routes: self.routes.iter().map(Route::service).collect(),
        })
    }
}

#[derive(derive_more::Debug)]
#[debug("ResourceRouter")]
pub struct ResourceRouter<St: AppState> {
    routes: Vec<RouteService<St>>,
    default: Option<HttpHandler<St>>,
}

impl<St: AppState> Service<St, WebRequest> for ResourceRouter<St> {
    type Res = WebResponse;
    type Error = St::Error;

    async fn call(
        &self,
        mut req: WebRequest,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        for route in &self.routes {
            if route.check(&mut req) {
                return ctx.call(route, req).await;
            }
        }
        if let Some(ref default) = self.default {
            ctx.call(default, req).await
        } else {
            Ok(WebResponse::new(
                Response::MethodNotAllowed().finish(),
                req.into_parts().0,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
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
                    .filter(async move |req: WebRequest| {
                        filter2.set(true);
                        Ok(req)
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
                .default_service(async move |r: WebRequest| {
                    Ok(r.into_response(HttpResponse::BadRequest()))
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
                    .default_service(async move |r: WebRequest| {
                        Ok(r.into_response(HttpResponse::BadRequest()))
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
