use std::{cell::RefCell, fmt, rc::Rc};

use crate::router::{IntoPattern, ResourceDef};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::cfg::SharedCfg;
use crate::service::dev::{AndThen, ServiceChain, ServiceChainFactory};
use crate::service::{Identity, IntoServiceFactory, Middleware, Service, ServiceFactory};
use crate::service::{ServiceCtx, chain, chain_factory};
use crate::{http::Response, util::Extensions};

use super::dev::{WebServiceConfig, WebServiceFactory, insert_slash};
use super::extract::FromRequest;
use super::handler::Handler;
use super::route::{IntoRoutes, Route, RouteService};
use super::stack::WebStack;
use super::{app::Filter, error::ErrorRenderer, guard::Guard, service::AppState};
use super::{request::WebRequest, response::WebResponse};

type HttpService<Err: ErrorRenderer> =
    BoxService<WebRequest<Err>, WebResponse, Err::Container>;
type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<SharedCfg, WebRequest<Err>, WebResponse, Err::Container, ()>;
type ResourcePipeline<F, Err> =
    ServiceChain<AndThen<F, ResourceRouter<Err>>, WebRequest<Err>>;

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
///             .route(web::get().to(|| async { HttpResponse::Ok() })));
/// }
/// ```
///
/// If no matching route could be found, *405* response code get returned.
/// Default behavior could be overriden with `default_resource()` method.
pub struct Resource<Err: ErrorRenderer, M = Identity, T = Filter<Err>> {
    middleware: M,
    filter: ServiceChainFactory<T, WebRequest<Err>, SharedCfg>,
    rdef: Vec<String>,
    name: Option<String>,
    routes: Vec<Route<Err>>,
    state: Option<Extensions>,
    guards: Vec<Box<dyn Guard>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
}

impl<Err: ErrorRenderer> Resource<Err> {
    #[allow(clippy::needless_pass_by_value)]
    pub fn new<T: IntoPattern>(path: T) -> Resource<Err> {
        Resource {
            routes: Vec::new(),
            rdef: path.patterns(),
            name: None,
            state: None,
            middleware: Identity,
            filter: chain_factory(Filter::new()),
            guards: Vec::new(),
            default: Rc::new(RefCell::new(None)),
        }
    }
}

impl<Err, M, T> Resource<Err, M, T>
where
    T: ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    Err: ErrorRenderer,
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
    ///                 .route(web::get().to(|| async { HttpResponse::MethodNotAllowed() }))
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
    ///                 .to(|| async { HttpResponse::Ok() }))
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
        R: IntoRoutes<Err>,
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
        F: Handler<Args, Err> + 'static,
        Args: FromRequest<Err> + 'static,
        Args::Error: Into<Err::Container>,
    {
        self.routes.push(Route::new().to(handler));
        self
    }

    #[must_use]
    /// Register request filter.
    ///
    /// This is similar to `App's` filters, but filter get invoked on resource level.
    pub fn filter<U, F>(
        self,
        filter: F,
    ) -> Resource<
        Err,
        M,
        impl ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    >
    where
        U: ServiceFactory<
                WebRequest<Err>,
                SharedCfg,
                Response = WebRequest<Err>,
                Error = Err::Container,
            >,
        F: IntoServiceFactory<U, WebRequest<Err>, SharedCfg>,
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
    pub fn middleware<U>(self, mw: U) -> Resource<Err, WebStack<M, U, Err>, T> {
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

    #[deprecated(since = "3.2.0", note = "use `middleware()` instead")]
    #[doc(hidden)]
    pub fn wrap<U>(self, mw: U) -> Resource<Err, WebStack<M, U, Err>, T> {
        self.middleware(mw)
    }

    #[must_use]
    /// Default service to be used if no matching route could be found.
    ///
    /// By default *405* response get returned. Resource does not use
    /// default handler from `App` or `Scope`.
    pub fn default_service<F, S>(mut self, f: F) -> Self
    where
        F: IntoServiceFactory<S, WebRequest<Err>, SharedCfg>,
        S: ServiceFactory<
                WebRequest<Err>,
                SharedCfg,
                Response = WebResponse,
                Error = Err::Container,
            > + 'static,
        S::InitError: fmt::Debug,
    {
        // create and configure default resource
        self.default = Rc::new(RefCell::new(Some(Rc::new(boxed::factory(
            chain_factory(f.into_factory())
                .map_init_err(|e| log::error!("Cannot construct default service: {e:?}")),
        )))));

        self
    }
}

impl<Err, M, T> WebServiceFactory<Err> for Resource<Err, M, T>
where
    T: ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    M: Middleware<ResourcePipeline<T::Service, Err>, SharedCfg> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    fn register(mut self, config: &mut WebServiceConfig<Err>) {
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

        let state = self.state.take().map(|state| {
            AppState::new(state, Some(config.state().clone()), config.state().config())
        });

        let router_factory = ResourceRouterFactory {
            state,
            routes: self.routes,
            default: self.default.borrow_mut().take(),
        };

        config.register_service(
            rdef,
            guards,
            ResourceServiceFactory {
                middleware: self.middleware,
                filter: self.filter,
                routing: router_factory,
            },
            None,
        );
    }
}

impl<Err, M, F>
    IntoServiceFactory<
        ResourceServiceFactory<Err, M, ServiceChainFactory<F, WebRequest<Err>, SharedCfg>>,
        WebRequest<Err>,
        SharedCfg,
    > for Resource<Err, M, F>
where
    F: ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    M: Middleware<ResourcePipeline<F::Service, Err>, SharedCfg> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    fn into_factory(
        self,
    ) -> ResourceServiceFactory<Err, M, ServiceChainFactory<F, WebRequest<Err>, SharedCfg>>
    {
        let router_factory = ResourceRouterFactory {
            state: None,
            routes: self.routes,
            default: self.default.borrow_mut().take(),
        };

        ResourceServiceFactory {
            middleware: self.middleware,
            filter: self.filter,
            routing: router_factory,
        }
    }
}

/// Resource service
pub struct ResourceServiceFactory<Err: ErrorRenderer, M, F> {
    middleware: M,
    filter: F,
    routing: ResourceRouterFactory<Err>,
}

impl<Err, M, F> ServiceFactory<WebRequest<Err>, SharedCfg>
    for ResourceServiceFactory<Err, M, F>
where
    M: Middleware<ResourcePipeline<F::Service, Err>, SharedCfg> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Service = M::Service;
    type InitError = ();

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let filter = self.filter.create(cfg).await?;
        let routing = self.routing.create(cfg).await?;
        Ok(self.middleware.create(chain(filter).and_then(routing), cfg))
    }
}

struct ResourceRouterFactory<Err: ErrorRenderer> {
    routes: Vec<Route<Err>>,
    default: Option<Rc<HttpNewService<Err>>>,
    state: Option<AppState>,
}

impl<Err: ErrorRenderer> ServiceFactory<WebRequest<Err>, SharedCfg>
    for ResourceRouterFactory<Err>
{
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = ResourceRouter<Err>;

    async fn create(&self, cfg: SharedCfg) -> Result<Self::Service, Self::InitError> {
        let default = if let Some(ref default) = self.default {
            Some(default.create(cfg).await?)
        } else {
            None
        };
        Ok(ResourceRouter {
            default,
            state: self.state.clone(),
            routes: self.routes.iter().map(Route::service).collect(),
        })
    }
}

pub struct ResourceRouter<Err: ErrorRenderer> {
    state: Option<AppState>,
    routes: Vec<RouteService<Err>>,
    default: Option<HttpService<Err>>,
}

impl<Err: ErrorRenderer> Service<WebRequest<Err>> for ResourceRouter<Err> {
    type Response = WebResponse;
    type Error = Err::Container;

    async fn call(
        &self,
        mut req: WebRequest<Err>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        for route in &self.routes {
            if route.check(&mut req) {
                if let Some(ref state) = self.state {
                    req.set_state_container(state.clone());
                }
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
    use crate::http::header::{self, HeaderValue};
    use crate::http::{Method, StatusCode};
    use crate::time::{Millis, sleep};
    use crate::web::middleware::DefaultHeaders;
    use crate::web::test::{TestRequest, call_service, init_service};
    use crate::web::{self, App, DefaultError, HttpResponse, guard, request::WebRequest};
    use crate::{service::fn_service, util::Ready};

    #[crate::rt_test]
    async fn test_filter() {
        let filter = std::rc::Rc::new(std::cell::Cell::new(false));
        let filter2 = filter.clone();
        let srv = init_service(
            App::new().service(
                web::resource("/test")
                    .filter(fn_service(move |req: WebRequest<_>| {
                        filter2.set(true);
                        Ready::Ok(req)
                    }))
                    .route(web::get().to(|| async { HttpResponse::Ok() })),
            ),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(filter.get());
    }

    #[crate::rt_test]
    #[allow(deprecated)]
    async fn test_middleware() {
        let srv = init_service(
            App::new().service(
                web::resource("/test")
                    .name("test")
                    .middleware(
                        DefaultHeaders::new()
                            .header(header::CONTENT_TYPE, HeaderValue::from_static("0001")),
                    )
                    .route(web::get().to(|| async { HttpResponse::Ok() })),
            ),
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
    async fn test_to() {
        let srv = init_service(App::new().service(web::resource("/test").to(|| async {
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
        let srv = init_service(App::new().service(
            web::resource(["/test", "/test2"]).to(|| async { HttpResponse::Ok() }),
        ))
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
                .service(
                    web::resource("/test")
                        .route(web::get().to(|| async { HttpResponse::Ok() })),
                )
                .default_service(|r: WebRequest<DefaultError>| async move {
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
                    .route(web::get().to(|| async { HttpResponse::Ok() }))
                    .default_service(|r: WebRequest<DefaultError>| async move {
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
                        .to(|| async { HttpResponse::Ok() }),
                )
                .service(
                    web::resource("/test/{p}")
                        .guard(guard::Put())
                        .to(|| async { HttpResponse::Created() }),
                )
                .service(
                    web::resource("/test/{p}")
                        .guard(guard::Delete())
                        .to(|| async { HttpResponse::NoContent() }),
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

    #[crate::rt_test]
    async fn test_state() {
        let srv = init_service(
            App::new().state(1i32).state(1usize).state('-').service(
                web::resource("/test")
                    .state(10usize)
                    .state('*')
                    .guard(guard::Get())
                    .to(
                        |data1: web::types::State<usize>,
                         data2: web::types::State<char>,
                         data3: web::types::State<i32>| {
                            assert_eq!(*data1, 10);
                            assert_eq!(*data2, '*');
                            assert_eq!(*data3, 1);
                            async { HttpResponse::Ok() }
                        },
                    ),
            ),
        )
        .await;

        let req = TestRequest::get().uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
