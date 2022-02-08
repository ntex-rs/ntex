use std::task::{Context, Poll};
use std::{convert::Infallible, future::Future, pin::Pin, rc::Rc};

use crate::http::Response;
use crate::router::{IntoPattern, ResourceDef};
use crate::service::{Identity, IntoServiceFactory, Service, ServiceFactory, Transform};
use crate::util::{ready, Extensions, Ready};

use super::dev::{insert_slash, WebService, WebServiceConfig};
use super::error::Error;
use super::extract::FromRequest;
use super::route::{IntoRoutes, Route, RouteService};
use super::stack::{
    Filter, Filters, FiltersFactory, Middleware, MiddlewareStack, Next, Stack,
};
use super::{guard::Guard, types::State, ErrorRenderer, Handler, WebRequest, WebResponse};

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
pub struct Resource<Err: ErrorRenderer, M = Identity, F = Filter> {
    filter: F,
    middleware: M,
    rdef: Vec<String>,
    name: Option<String>,
    routes: Vec<Route<Err>>,
    state: Option<Extensions>,
    guards: Vec<Box<dyn Guard>>,
    default: Route<Err>,
}

impl<Err: ErrorRenderer> Resource<Err> {
    pub fn new<T: IntoPattern>(path: T) -> Resource<Err> {
        Resource {
            routes: Vec::new(),
            rdef: path.patterns(),
            name: None,
            filter: Filter,
            middleware: Identity,
            guards: Vec::new(),
            state: None,
            default: Route::new().to(|| async { Response::MethodNotAllowed().finish() }),
        }
    }
}

impl<'a, Err, M, F> Resource<Err, M, F>
where
    Err: ErrorRenderer,
{
    /// Set resource name.
    ///
    /// Name is used for url generation.
    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.to_string());
        self
    }

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
        R: IntoRoutes<'a, Err>,
    {
        for route in route.routes() {
            self.routes.push(route);
        }
        self
    }

    /// Provide resource specific state. This method allows to add extractor
    /// configuration or specific state available via `State<T>` extractor.
    /// Provided state is available for all routes registered for the current resource.
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
    ///           .app_state(web::types::PayloadConfig::new(4096))
    ///           .route(
    ///               web::get()
    ///                  // register handler
    ///                  .to(index)
    ///           ));
    /// }
    /// ```
    pub fn state<D: 'static>(self, st: D) -> Self {
        self.app_state(State::new(st))
    }

    /// Set or override application state.
    ///
    /// This method overrides state stored with [`App::app_state()`](#method.app_state)
    pub fn app_state<D: 'static>(mut self, st: D) -> Self {
        if self.state.is_none() {
            self.state = Some(Extensions::new());
        }
        self.state.as_mut().unwrap().insert(st);
        self
    }

    /// Register a new route and add handler. This route matches all requests.
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
    pub fn to<H, Args>(mut self, handler: H) -> Self
    where
        H: Handler<'a, Args, Err>,
        Args: FromRequest<'a, Err> + 'a,
        Args::Error: Error<Err>,
    {
        self.routes.push(Route::new().to(handler));
        self
    }

    /// Register request filter.
    ///
    /// This is similar to `App's` filters, but filter get invoked on resource level.
    pub fn filter<U>(self, filter: U) -> Resource<Err, M, Filters<F, U>> {
        Resource {
            filter: Filters::new(self.filter, filter),
            middleware: self.middleware,
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
            state: self.state,
        }
    }

    /// Register a resource middleware.
    ///
    /// This is similar to `App's` middlewares, but middleware get invoked on resource level.
    /// Resource level middlewares are not allowed to change response
    /// type (i.e modify response's body).
    ///
    /// **Note**: middlewares get called in opposite order of middlewares registration.
    pub fn wrap<U>(self, mw: U) -> Resource<Err, Stack<M, U>, F> {
        Resource {
            middleware: Stack::new(self.middleware, mw),
            filter: self.filter,
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
            state: self.state,
        }
    }

    /// Default handler to be used if no matching route could be found.
    /// By default *405* response get returned. Resource does not use
    /// default handler from `App` or `Scope`.
    pub fn default<H, Args>(mut self, handler: H) -> Self
    where
        H: Handler<'a, Args, Err>,
        Args: FromRequest<'a, Err> + 'a,
        Args::Error: Error<Err>,
    {
        self.default = Route::new().to(handler);
        self
    }
}

impl<'a, Err, M, F> WebService<'a, Err> for Resource<Err, M, F>
where
    M: Transform<
            Next<
                ResourceService<
                    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service,
                    Err,
                >,
            >,
        > + 'static,
    M::Service:
        Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Infallible>,
    F: FiltersFactory<'a, Err> + 'static,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service: 'static,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Future: 'static,
    Err: ErrorRenderer,
{
    fn register(mut self, config: &mut WebServiceConfig<'a, Err>) {
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
            *rdef.name_mut() = name.clone();
        }
        // custom app data storage
        if let Some(ref mut ext) = self.state {
            config.set_service_state(ext);
        }

        let router_factory = ResourceRouterFactory {
            routes: self.routes,
            state: self.state.map(Rc::new),
            default: self.default,
        };

        config.register_service(
            rdef,
            guards,
            ResourceServiceFactory {
                middleware: Rc::new(MiddlewareStack::new(self.middleware)),
                filter: self.filter.create(),
                routing: router_factory,
            },
            None,
        )
    }
}

impl<'a, Err, M, F>
    IntoServiceFactory<
        ResourceServiceFactory<Err, M, F::Service>,
        &'a mut WebRequest<'a, Err>,
    > for Resource<Err, M, F>
where
    M: Transform<
            Next<
                ResourceService<
                    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service,
                    Err,
                >,
            >,
        > + 'static,
    M::Service:
        Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Infallible>,
    F: FiltersFactory<'a, Err> + 'static,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Service: 'static,
    <F::Service as ServiceFactory<&'a mut WebRequest<'a, Err>>>::Future: 'static,
    Err: ErrorRenderer,
{
    fn into_factory(self) -> ResourceServiceFactory<Err, M, F::Service> {
        let router_factory = ResourceRouterFactory {
            routes: self.routes,
            state: self.state.map(Rc::new),
            default: self.default,
        };

        ResourceServiceFactory {
            middleware: Rc::new(MiddlewareStack::new(self.middleware)),
            filter: self.filter.create(),
            routing: router_factory,
        }
    }
}

/// Resource service
pub struct ResourceServiceFactory<Err: ErrorRenderer, M, F> {
    middleware: Rc<MiddlewareStack<M, Err>>,
    filter: F,
    routing: ResourceRouterFactory<Err>,
}

impl<'a, Err, M, F> ServiceFactory<&'a mut WebRequest<'a, Err>>
    for ResourceServiceFactory<Err, M, F>
where
    M: Transform<Next<ResourceService<F::Service, Err>>> + 'static,
    M::Service:
        Service<&'a mut WebRequest<'a, Err>, Response = WebResponse, Error = Infallible>,
    F: ServiceFactory<
            &'a mut WebRequest<'a, Err>,
            Response = &'a mut WebRequest<'a, Err>,
            Error = Infallible,
            InitError = (),
        > + 'static,
    F::Service: 'static,
    F::Future: 'static,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Infallible;
    type Service = Middleware<M::Service, Err>;
    type InitError = ();
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let filter_fut = self.filter.new_service(());
        let routing_fut = self.routing.new_service(());
        let middleware = self.middleware.clone();
        Box::pin(async move {
            Ok(middleware.new_transform(Next::new(ResourceService {
                filter: filter_fut.await?,
                routing: Rc::new(routing_fut.await?),
            })))
        })
    }
}

pub struct ResourceService<F, Err: ErrorRenderer> {
    filter: F,
    routing: Rc<ResourceRouter<Err>>,
}

impl<'a, F, Err> Service<&'a mut WebRequest<'a, Err>> for ResourceService<F, Err>
where
    F: Service<
        &'a mut WebRequest<'a, Err>,
        Response = &'a mut WebRequest<'a, Err>,
        Error = Infallible,
    >,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Infallible;
    type Future = ResourceServiceResponse<'a, F, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready1 = self.filter.poll_ready(cx).is_ready();
        let ready2 = self.routing.poll_ready(cx).is_ready();
        if ready1 && ready2 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&self, req: &'a mut WebRequest<'a, Err>) -> Self::Future {
        ResourceServiceResponse {
            filter: self
                .filter
                .call(unsafe { (req as *mut WebRequest<'a, Err>).as_mut().unwrap() }),
            routing: self.routing.clone(),
            endpoint: None,
            req,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct ResourceServiceResponse<'a, F: Service<&'a mut WebRequest<'a, Err>>, Err: ErrorRenderer> {
        #[pin]
        filter: F::Future,
        routing: Rc<ResourceRouter<Err>>,
        endpoint: Option<<ResourceRouter<Err> as Service<&'a mut WebRequest<'a, Err>>>::Future>,
        req: &'a mut WebRequest<'a, Err>,
    }
}

impl<'a, F, Err> Future for ResourceServiceResponse<'a, F, Err>
where
    F: Service<
        &'a mut WebRequest<'a, Err>,
        Response = &'a mut WebRequest<'a, Err>,
        Error = Infallible,
    >,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Infallible>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        if let Some(fut) = this.endpoint.as_mut() {
            Pin::new(fut).poll(cx)
        } else {
            let req = ready!(this.filter.poll(cx)).unwrap();
            *this.endpoint = Some(this.routing.call(req));
            self.poll(cx)
        }
    }
}

struct ResourceRouterFactory<Err: ErrorRenderer> {
    routes: Vec<Route<Err>>,
    state: Option<Rc<Extensions>>,
    default: Route<Err>,
}

impl<'a, Err: ErrorRenderer> ServiceFactory<&'a mut WebRequest<'a, Err>>
    for ResourceRouterFactory<Err>
{
    type Response = WebResponse;
    type Error = Infallible;
    type InitError = ();
    type Service = ResourceRouter<Err>;
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        let state = self.state.clone();
        let routes = self.routes.iter().map(|route| route.service()).collect();
        let default = self.default.service();

        Ready::Ok(ResourceRouter {
            routes,
            state,
            default,
        })
    }
}

struct ResourceRouter<Err: ErrorRenderer> {
    routes: Vec<RouteService<Err>>,
    state: Option<Rc<Extensions>>,
    default: RouteService<Err>,
}

impl<'a, Err: ErrorRenderer> Service<&'a mut WebRequest<'a, Err>> for ResourceRouter<Err> {
    type Response = WebResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<WebResponse, Infallible>> + 'a>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, req: &'a mut WebRequest<'a, Err>) -> Self::Future {
        for route in self.routes.iter() {
            if route.check(req) {
                if let Some(ref state) = self.state {
                    req.set_state_container(state.clone());
                }
                return route.call(req);
            }
        }
        self.default.call(req)
    }
}

#[cfg(test)]
mod tests {
    use crate::http::header::{self, HeaderValue};
    use crate::http::{Method, StatusCode};
    use crate::time::{sleep, Millis};
    use crate::web::middleware::DefaultHeaders;
    use crate::web::test::{call_service, init_service, TestRequest};
    use crate::web::{self, guard, request::WebRequest, App, DefaultError, HttpResponse};
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
    async fn test_middleware() {
        let srv = init_service(
            App::new().service(
                web::resource("/test")
                    .name("test")
                    .wrap(
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
            App::new()
                .state(1i32)
                .state(1usize)
                .app_state(web::types::State::new('-'))
                .service(
                    web::resource("/test")
                        .state(10usize)
                        .app_state(web::types::State::new('*'))
                        .guard(guard::Get())
                        .to(
                            |data1: web::types::State<usize>,
                             data2: web::types::State<char>,
                             data3: web::types::State<i32>| {
                                assert_eq!(**data1, 10);
                                assert_eq!(**data2, '*');
                                assert_eq!(**data3, 1);
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
