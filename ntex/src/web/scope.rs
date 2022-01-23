use std::{
    cell::RefCell, fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use crate::http::Response;
use crate::router::{IntoPattern, ResourceDef, Router};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::{pipeline_factory, PipelineFactory};
use crate::service::{Identity, IntoServiceFactory, Service, ServiceFactory, Transform};
use crate::util::{Either, Extensions, Ready};

use super::app::{Filter, Stack};
use super::config::ServiceConfig;
use super::dev::{WebServiceConfig, WebServiceFactory};
use super::error::ErrorRenderer;
use super::guard::Guard;
use super::request::WebRequest;
use super::resource::Resource;
use super::response::WebResponse;
use super::rmap::ResourceMap;
use super::route::Route;
use super::service::{AppServiceFactory, ServiceFactoryWrapper};
use super::types::State;

type Guards = Vec<Box<dyn Guard>>;
type HttpService<Err: ErrorRenderer> =
    BoxService<WebRequest<Err>, WebResponse, Err::Container>;
type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<(), WebRequest<Err>, WebResponse, Err::Container, ()>;
type BoxResponse<Err: ErrorRenderer> =
    Pin<Box<dyn Future<Output = Result<WebResponse, Err::Container>>>>;

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
///     let app = App::new().service(
///         web::scope("/{project_id}/")
///             .service(web::resource("/path1").to(|| async { HttpResponse::Ok() }))
///             .service(web::resource("/path2").route(web::get().to(|| async { HttpResponse::Ok() })))
///             .service(web::resource("/path3").route(web::head().to(|| async { HttpResponse::MethodNotAllowed() })))
///     );
/// }
/// ```
///
/// In the above example three routes get registered:
///  * /{project_id}/path1 - reponds to all http method
///  * /{project_id}/path2 - `GET` requests
///  * /{project_id}/path3 - `HEAD` requests
///
pub struct Scope<Err: ErrorRenderer, M = Identity, T = Filter<Err>> {
    middleware: M,
    filter: PipelineFactory<T, WebRequest<Err>>,
    rdef: Vec<String>,
    state: Option<Extensions>,
    services: Vec<Box<dyn AppServiceFactory<Err>>>,
    guards: Vec<Box<dyn Guard>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
    external: Vec<ResourceDef>,
    case_insensitive: bool,
}

impl<Err: ErrorRenderer> Scope<Err> {
    /// Create a new scope
    pub fn new<T: IntoPattern>(path: T) -> Scope<Err> {
        Scope {
            middleware: Identity,
            filter: pipeline_factory(Filter::new()),
            rdef: path.patterns(),
            state: None,
            guards: Vec::new(),
            services: Vec::new(),
            default: Rc::new(RefCell::new(None)),
            external: Vec::new(),
            case_insensitive: false,
        }
    }
}

impl<Err, M, T> Scope<Err, M, T>
where
    T: ServiceFactory<
        WebRequest<Err>,
        Response = WebRequest<Err>,
        Error = Err::Container,
        InitError = (),
    >,
    Err: ErrorRenderer,
{
    /// Add match guard to a scope.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App, HttpRequest, HttpResponse};
    ///
    /// async fn index(data: web::types::Path<(String, String)>) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app")
    ///             .guard(guard::Header("content-type", "text/plain"))
    ///             .route("/test1", web::get().to(index))
    ///             .route("/test2", web::post().to(|r: HttpRequest| async {
    ///                 HttpResponse::MethodNotAllowed()
    ///             }))
    ///     );
    /// }
    /// ```
    pub fn guard<G: Guard + 'static>(mut self, guard: G) -> Self {
        self.guards.push(Box::new(guard));
        self
    }

    /// Set or override application state. Application state could be accessed
    /// by using `State<T>` extractor where `T` is state type.
    ///
    /// ```rust
    /// use std::cell::Cell;
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// struct MyState {
    ///     counter: Cell<usize>,
    /// }
    ///
    /// async fn index(st: web::types::State<MyState>) -> HttpResponse {
    ///     st.counter.set(st.counter.get() + 1);
    ///     HttpResponse::Ok().into()
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app")
    ///             .state(MyState{ counter: Cell::new(0) })
    ///             .service(
    ///                 web::resource("/index.html").route(
    ///                     web::get().to(index)))
    ///     );
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

    /// Use ascii case-insensitive routing.
    ///
    /// Only static segments could be case-insensitive.
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
    /// ```rust
    /// use ntex::web::{self, middleware, App, HttpResponse};
    ///
    /// // this function could be located in different module
    /// fn config(cfg: &mut web::ServiceConfig) {
    ///     cfg.service(web::resource("/test")
    ///         .route(web::get().to(|| async { HttpResponse::Ok() }))
    ///         .route(web::head().to(|| async { HttpResponse::MethodNotAllowed() }))
    ///     );
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .wrap(middleware::Logger::default())
    ///         .service(
    ///             web::scope("/api")
    ///                 .configure(config)
    ///         )
    ///         .route("/index.html", web::get().to(|| async { HttpResponse::Ok() }));
    /// }
    /// ```
    pub fn configure<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut ServiceConfig<Err>),
    {
        let mut cfg = ServiceConfig::new();
        f(&mut cfg);
        self.services.extend(cfg.services);
        self.external.extend(cfg.external);

        if !cfg.state.is_empty() {
            let mut state = self.state.unwrap_or_else(Extensions::new);

            for value in cfg.state.iter() {
                value.create(&mut state);
            }

            self.state = Some(state);
        }
        self
    }

    /// Register http service.
    ///
    /// This is similar to `App's` service registration.
    ///
    /// ntex web provides several services implementations:
    ///
    /// * *Resource* is an entry in resource table which corresponds to requested URL.
    /// * *Scope* is a set of resources with common root path.
    /// * "StaticFiles" is a service for static files support
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest};
    ///
    /// struct AppState;
    ///
    /// async fn index(req: HttpRequest) -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app").service(
    ///             web::scope("/v1")
    ///                 .service(web::resource("/test1").to(index)))
    ///     );
    /// }
    /// ```
    pub fn service<F>(mut self, factory: F) -> Self
    where
        F: WebServiceFactory<Err> + 'static,
    {
        self.services
            .push(Box::new(ServiceFactoryWrapper::new(factory)));
        self
    }

    /// Configure route for a specific path.
    ///
    /// This is a simplified version of the `Scope::service()` method.
    /// This method can be called multiple times, in that case
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
    ///     let app = App::new().service(
    ///         web::scope("/app")
    ///             .route("/test1", web::get().to(index))
    ///             .route("/test2", web::post().to(|| async { HttpResponse::MethodNotAllowed() }))
    ///     );
    /// }
    /// ```
    pub fn route(self, path: &str, mut route: Route<Err>) -> Self {
        self.service(
            Resource::new(path)
                .add_guards(route.take_guards())
                .route(route),
        )
    }

    /// Default service to be used if no matching route could be found.
    ///
    /// If default resource is not registered, app's default resource is being used.
    pub fn default_service<F, S>(mut self, f: F) -> Self
    where
        F: IntoServiceFactory<S, WebRequest<Err>>,
        S: ServiceFactory<WebRequest<Err>, Response = WebResponse, Error = Err::Container>
            + 'static,
        S::InitError: fmt::Debug,
    {
        // create and configure default resource
        self.default = Rc::new(RefCell::new(Some(Rc::new(boxed::factory(
            f.into_factory()
                .map_init_err(|e| log::error!("Cannot construct default service: {:?}", e)),
        )))));

        self
    }

    /// Register request filter.
    ///
    /// Filter runs during inbound processing in the request
    /// lifecycle (request -> response), modifying request as
    /// necessary, across all requests managed by the *Scope*.
    ///
    /// This is similar to `App's` filters, but filter get invoked on scope level.
    pub fn filter<U, F>(
        self,
        filter: F,
    ) -> Scope<
        Err,
        M,
        impl ServiceFactory<
            WebRequest<Err>,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    >
    where
        U: ServiceFactory<
            WebRequest<Err>,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
        F: IntoServiceFactory<U, WebRequest<Err>>,
    {
        Scope {
            filter: self.filter.and_then(filter.into_factory()),
            middleware: self.middleware,
            rdef: self.rdef,
            state: self.state,
            guards: self.guards,
            services: self.services,
            default: self.default,
            external: self.external,
            case_insensitive: self.case_insensitive,
        }
    }

    /// Registers middleware, in the form of a middleware component (type).
    ///
    /// That runs during inbound processing in the request
    /// lifecycle (request -> response), modifying request as
    /// necessary, across all requests managed by the *Scope*. Scope-level
    /// middleware is more limited in what it can modify, relative to Route or
    /// Application level middleware, in that Scope-level middleware can not modify
    /// WebResponse.
    ///
    /// Use middleware when you need to read or modify *every* request in some way.
    pub fn wrap<U>(self, mw: U) -> Scope<Err, Stack<M, U>, T> {
        Scope {
            middleware: Stack::new(self.middleware, mw),
            filter: self.filter,
            rdef: self.rdef,
            state: self.state,
            guards: self.guards,
            services: self.services,
            default: self.default,
            external: self.external,
            case_insensitive: self.case_insensitive,
        }
    }
}

impl<Err, M, T> WebServiceFactory<Err> for Scope<Err, M, T>
where
    T: ServiceFactory<
            WebRequest<Err>,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    M: Transform<ScopeService<T::Service, Err>> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    fn register(mut self, config: &mut WebServiceConfig<Err>) {
        // update default resource if needed
        if self.default.borrow().is_none() {
            *self.default.borrow_mut() = Some(config.default_service());
        }

        // register nested services
        let mut cfg = config.clone_config();
        self.services
            .into_iter()
            .for_each(|mut srv| srv.register(&mut cfg));

        let slesh = self.rdef.iter().any(|s| s.ends_with('/'));
        let mut rmap = ResourceMap::new(ResourceDef::root_prefix(self.rdef.clone()));

        // external resources
        for mut rdef in std::mem::take(&mut self.external) {
            rmap.add(&mut rdef, None);
        }

        // custom app data storage
        if let Some(ref mut ext) = self.state {
            config.set_service_state(ext);
        }

        // complete scope pipeline creation
        let router_factory = ScopeRouterFactory {
            state: self.state.take().map(Rc::new),
            default: self.default.clone(),
            case_insensitive: self.case_insensitive,
            services: Rc::new(
                cfg.into_services()
                    .1
                    .into_iter()
                    .map(|(rdef, srv, guards, nested)| {
                        // case for scope prefix ends with '/' and
                        // resource is empty pattern
                        let mut rdef = if slesh && rdef.pattern() == "" {
                            ResourceDef::new("/")
                        } else {
                            rdef
                        };
                        rmap.add(&mut rdef, nested);
                        (rdef, srv, RefCell::new(guards))
                    })
                    .collect(),
            ),
        };

        // get guards
        let guards = if self.guards.is_empty() {
            None
        } else {
            Some(self.guards)
        };

        // register final service
        config.register_service(
            ResourceDef::root_prefix(self.rdef),
            guards,
            ScopeServiceFactory {
                middleware: Rc::new(self.middleware),
                filter: self.filter,
                routing: router_factory,
            },
            Some(Rc::new(rmap)),
        )
    }
}

/// Scope service
struct ScopeServiceFactory<M, F, Err: ErrorRenderer> {
    middleware: Rc<M>,
    filter: F,
    routing: ScopeRouterFactory<Err>,
}

impl<M, F, Err> ServiceFactory<WebRequest<Err>> for ScopeServiceFactory<M, F, Err>
where
    M: Transform<ScopeService<F::Service, Err>> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            WebRequest<Err>,
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
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let filter_fut = self.filter.new_service(());
        let routing_fut = self.routing.new_service(());
        let middleware = self.middleware.clone();
        Box::pin(async move {
            Ok(middleware.new_transform(ScopeService {
                filter: filter_fut.await?,
                routing: Rc::new(routing_fut.await?),
            }))
        })
    }
}

pub struct ScopeService<F, Err: ErrorRenderer> {
    filter: F,
    routing: Rc<ScopeRouter<Err>>,
}

impl<F, Err> Service<WebRequest<Err>> for ScopeService<F, Err>
where
    F: Service<WebRequest<Err>, Response = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = ScopeServiceResponse<F, Err>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready1 = self.filter.poll_ready(cx)?.is_ready();
        let ready2 = self.routing.poll_ready(cx)?.is_ready();
        if ready1 && ready2 {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(&self, req: WebRequest<Err>) -> Self::Future {
        ScopeServiceResponse {
            filter: self.filter.call(req),
            routing: self.routing.clone(),
            endpoint: None,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct ScopeServiceResponse<F: Service<WebRequest<Err>>, Err: ErrorRenderer> {
        #[pin]
        filter: F::Future,
        routing: Rc<ScopeRouter<Err>>,
        endpoint: Option<<ScopeRouter<Err> as Service<WebRequest<Err>>>::Future>,
    }
}

impl<F, Err> Future for ScopeServiceResponse<F, Err>
where
    F: Service<WebRequest<Err>, Response = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Output = Result<WebResponse, Err::Container>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        if let Some(fut) = this.endpoint.as_mut() {
            Pin::new(fut).poll(cx)
        } else {
            let res = if let Poll::Ready(res) = this.filter.poll(cx) {
                res?
            } else {
                return Poll::Pending;
            };
            *this.endpoint = Some(this.routing.call(res));
            self.poll(cx)
        }
    }
}

struct ScopeRouterFactory<Err: ErrorRenderer> {
    state: Option<Rc<Extensions>>,
    services: Rc<Vec<(ResourceDef, HttpNewService<Err>, RefCell<Option<Guards>>)>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
    case_insensitive: bool,
}

impl<Err: ErrorRenderer> ServiceFactory<WebRequest<Err>> for ScopeRouterFactory<Err> {
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = ScopeRouter<Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let services = self.services.clone();
        let case_insensitive = self.case_insensitive;
        let state = self.state.clone();
        let default_fut = self
            .default
            .borrow()
            .as_ref()
            .map(|srv| srv.new_service(()));

        Box::pin(async move {
            // create http services
            let mut router = Router::build();
            if case_insensitive {
                router.case_insensitive();
            }
            for (path, factory, guards) in &mut services.iter() {
                let service = factory.new_service(()).await?;
                router.rdef(path.clone(), service).2 = guards.borrow_mut().take();
            }

            let default = if let Some(fut) = default_fut {
                Some(fut.await?)
            } else {
                None
            };

            Ok(ScopeRouter {
                state,
                default,
                router: router.finish(),
            })
        })
    }
}

struct ScopeRouter<Err: ErrorRenderer> {
    state: Option<Rc<Extensions>>,
    router: Router<HttpService<Err>, Vec<Box<dyn Guard>>>,
    default: Option<HttpService<Err>>,
}

impl<Err: ErrorRenderer> Service<WebRequest<Err>> for ScopeRouter<Err> {
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = Either<BoxResponse<Err>, Ready<Self::Response, Self::Error>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, mut req: WebRequest<Err>) -> Self::Future {
        let res = self.router.recognize_checked(&mut req, |req, guards| {
            if let Some(guards) = guards {
                for f in guards {
                    if !f.check(req.head()) {
                        return false;
                    }
                }
            }
            true
        });

        if let Some((srv, _info)) = res {
            if let Some(ref state) = self.state {
                req.set_state_container(state.clone());
            }
            Either::Left(srv.call(req))
        } else if let Some(ref default) = self.default {
            Either::Left(default.call(req))
        } else {
            let req = req.into_parts().0;
            Either::Right(Ready::Ok(WebResponse::new(
                Response::NotFound().finish(),
                req,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::http::body::{Body, ResponseBody};
    use crate::http::header::{HeaderValue, CONTENT_TYPE};
    use crate::http::{Method, StatusCode};
    use crate::service::{fn_service, Service};
    use crate::util::{Bytes, Ready};
    use crate::web::middleware::DefaultHeaders;
    use crate::web::request::WebRequest;
    use crate::web::test::{call_service, init_service, read_body, TestRequest};
    use crate::web::DefaultError;
    use crate::web::{self, guard, App, HttpRequest, HttpResponse};

    #[crate::rt_test]
    async fn test_scope() {
        let srv = init_service(
            App::new().service(
                web::scope("/app")
                    .service(web::resource("/path1").to(|| async { HttpResponse::Ok() })),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/app/path10").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[crate::rt_test]
    async fn test_scope_root() {
        let srv = init_service(
            App::new().service(
                web::scope("/app")
                    .service(web::resource("").to(|| async { HttpResponse::Ok() }))
                    .service(web::resource("/").to(|| async { HttpResponse::Created() })),
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
                    .service(web::resource("").to(|| async { HttpResponse::Ok() }))
                    .service(web::resource("/").to(|| async { HttpResponse::Created() })),
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
        let srv = init_service(
            App::new().service(
                web::scope("/app/")
                    .service(web::resource("").to(|| async { HttpResponse::Ok() })),
            ),
        )
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
                    .service(web::resource("").to(|| async { HttpResponse::Ok() })),
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
        let srv = init_service(
            App::new().service(
                web::scope("/app/")
                    .service(web::resource("/").to(|| async { HttpResponse::Ok() })),
            ),
        )
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
                    .route("/path1", web::get().to(|| async { HttpResponse::Ok() }))
                    .route("/path1", web::delete().to(|| async { HttpResponse::Ok() })),
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
                    .route("/path1", web::get().to(|| async { HttpResponse::Ok() }))
                    .route("/path1", web::delete().to(|| async { HttpResponse::Ok() })),
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
                        .route(web::get().to(|| async { HttpResponse::Ok() }))
                        .route(web::delete().to(|| async { HttpResponse::Ok() })),
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
                    web::scope("/app").guard(guard::Get()).service(
                        web::resource("/path1").to(|| async { HttpResponse::Ok() }),
                    ),
                )
                .service(web::scope("/app").guard(guard::Post()).service(
                    web::resource("/path1").to(|| async { HttpResponse::NotModified() }),
                ))
                .service(
                    web::resource("/app/path1").to(|| async { HttpResponse::NoContent() }),
                ),
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
            web::resource("/path1").to(|r: HttpRequest| async move {
                HttpResponse::Ok().body(format!("project: {}", &r.match_info()["project"]))
            }),
        )))
        .await;

        let req = TestRequest::with_uri("/ab-project1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        if let ResponseBody::Body(Body::Bytes(ref b)) = resp.response().body() {
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
            web::resource(["", "/"]).to(|r: HttpRequest| async move {
                HttpResponse::Ok().body(format!("project: {}", &r.match_info()["project"]))
            }),
        )))
        .await;

        let req = TestRequest::with_uri("/ab-project1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        if let ResponseBody::Body(Body::Bytes(ref b)) = resp.response().body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: project1"));
        }

        let req = TestRequest::with_uri("/ab-project1/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        if let ResponseBody::Body(Body::Bytes(ref b)) = resp.response().body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: project1"));
        }

        let req = TestRequest::with_uri("/aa-project1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[crate::rt_test]
    async fn test_nested_scope() {
        let srv =
            init_service(App::new().service(web::scope("/app").service(
                web::scope("/t1").service(
                    web::resource("/path1").to(|| async { HttpResponse::Created() }),
                ),
            )))
            .await;

        let req = TestRequest::with_uri("/app/t1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[crate::rt_test]
    async fn test_nested_scope_no_slash() {
        let srv =
            init_service(App::new().service(web::scope("/app").service(
                web::scope("t1").service(
                    web::resource("/path1").to(|| async { HttpResponse::Created() }),
                ),
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
                        .service(web::resource("").to(|| async { HttpResponse::Ok() }))
                        .service(
                            web::resource("/").to(|| async { HttpResponse::Created() }),
                        ),
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
        let srv =
            init_service(App::new().service(
                web::scope("/app").service(
                    web::scope("/t1").guard(guard::Get()).service(
                        web::resource("/path1").to(|| async { HttpResponse::Ok() }),
                    ),
                ),
            ))
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
                |r: HttpRequest| async move {
                    HttpResponse::Created()
                        .body(format!("project: {}", &r.match_info()["project_id"]))
                },
            )),
        )))
        .await;

        let req = TestRequest::with_uri("/app/project_1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        if let ResponseBody::Body(Body::Bytes(ref b)) = resp.response().body() {
            let bytes: Bytes = b.clone();
            assert_eq!(bytes, Bytes::from_static(b"project: project_1"));
        }
    }

    #[crate::rt_test]
    async fn test_nested2_scope_with_variable_segment() {
        let srv = init_service(App::new().service(web::scope("/app").service(
            web::scope("/{project}").service(web::scope("/{id}").service(
                web::resource("/path1").to(|r: HttpRequest| async move {
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

        if let ResponseBody::Body(Body::Bytes(ref b)) = resp.response().body() {
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
                    .service(web::resource("/path1").to(|| async { HttpResponse::Ok() }))
                    .default_service(|r: WebRequest<DefaultError>| async move {
                        Ok(r.into_response(HttpResponse::BadRequest()))
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
    async fn test_default_resource_propagation() {
        let srv = init_service(
            App::new()
                .service(web::scope("/app1").default_service(
                    web::resource("").to(|| async { HttpResponse::BadRequest() }),
                ))
                .service(web::scope("/app2"))
                .default_service(|r: WebRequest<DefaultError>| async move {
                    Ok(r.into_response(HttpResponse::MethodNotAllowed()))
                }),
        )
        .await;

        let req = TestRequest::with_uri("/non-exist").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let req = TestRequest::with_uri("/app1/non-exist").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let req = TestRequest::with_uri("/app2/non-exist").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[crate::rt_test]
    async fn test_filter() {
        let filter = std::rc::Rc::new(std::cell::Cell::new(false));
        let filter2 = filter.clone();
        let srv = init_service(
            App::new().service(
                web::scope("app")
                    .filter(fn_service(move |req: WebRequest<_>| {
                        filter2.set(true);
                        Ready::Ok(req)
                    }))
                    .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
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
                    .wrap(
                        DefaultHeaders::new()
                            .header(CONTENT_TYPE, HeaderValue::from_static("0001")),
                    )
                    .service(
                        web::resource("/test")
                            .route(web::get().to(|| async { HttpResponse::Ok() })),
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
    async fn test_override_data() {
        let srv = init_service(App::new().state(1usize).service(
            web::scope("app").state(10usize).route(
                "/t",
                web::get().to(|data: web::types::State<usize>| {
                    assert_eq!(**data, 10);
                    async { HttpResponse::Ok() }
                }),
            ),
        ))
        .await;

        let req = TestRequest::with_uri("/app/t").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_override_app_data() {
        let srv = init_service(
            App::new()
                .app_state(web::types::State::new(1usize))
                .service(
                    web::scope("app")
                        .app_state(web::types::State::new(10usize))
                        .route(
                            "/t",
                            web::get().to(|data: web::types::State<usize>| {
                                assert_eq!(**data, 10);
                                async { HttpResponse::Ok() }
                            }),
                        ),
                ),
        )
        .await;

        let req = TestRequest::with_uri("/app/t").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_scope_config() {
        let srv = init_service(App::new().service(web::scope("/app").configure(|s| {
            s.state("teat");
            s.route("/path1", web::get().to(|| async { HttpResponse::Ok() }));
        })))
        .await;

        let req = TestRequest::with_uri("/app/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_scope_config_2() {
        let srv = init_service(App::new().service(web::scope("/app").configure(|s| {
            s.service(web::scope("/v1").configure(|s| {
                s.route("/", web::get().to(|| async { HttpResponse::Ok() }));
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
                    web::get().to(|req: HttpRequest| async move {
                        HttpResponse::Ok().body(
                            req.url_for("youtube", &["xxxxxx"])
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
            web::scope("/b").service(web::resource("/c/{stuff}").name("c").route(
                web::get().to(|req: HttpRequest| async move {
                    HttpResponse::Ok()
                        .body(format!("{}", req.url_for("c", &["12345"]).unwrap()))
                }),
            )),
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
