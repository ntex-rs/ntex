use std::{cell::Cell, cell::RefCell, fmt, marker::PhantomData, rc::Rc};

use crate::http::Request;
use crate::router::ResourceDef;
use crate::service::boxed::{self, BoxServiceFactory};
use crate::service::cfg::SharedCfg;
use crate::service::{Identity, Middleware, Service, ServiceCtx, ServiceFactory};
use crate::service::{IntoServiceFactory, chain_factory, dev::ServiceChainFactory};
use crate::util::{BoxFuture, Extensions};

use super::app_service::{AppFactory, AppService};
use super::config::ServiceConfig;
use super::request::WebRequest;
use super::resource::Resource;
use super::response::WebResponse;
use super::route::Route;
use super::service::{AppServiceFactory, ServiceFactoryWrapper, WebServiceFactory};
use super::stack::WebStack;
use super::{DefaultError, ErrorRenderer};

type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<SharedCfg, WebRequest<Err>, WebResponse, Err::Container, ()>;
type FnStateFactory = Box<dyn Fn(Extensions) -> BoxFuture<'static, Result<Extensions, ()>>>;

/// Application builder - structure that follows the builder pattern
/// for building application instances.
pub struct App<M, F, Err: ErrorRenderer = DefaultError> {
    middleware: M,
    filter: ServiceChainFactory<F, WebRequest<Err>, SharedCfg>,
    services: Vec<Box<dyn AppServiceFactory<Err>>>,
    default: Option<Rc<HttpNewService<Err>>>,
    external: Vec<ResourceDef>,
    extensions: Extensions,
    state_factories: Vec<FnStateFactory>,
    error_renderer: Err,
    case_insensitive: bool,
}

impl Default for App<Identity, Filter<DefaultError>, DefaultError> {
    fn default() -> Self {
        Self::new()
    }
}

impl App<Identity, Filter<DefaultError>, DefaultError> {
    #[must_use]
    /// Create application builder. Application can be configured with a builder-like pattern.
    pub fn new() -> Self {
        App {
            middleware: Identity,
            filter: chain_factory(Filter::new()),
            state_factories: Vec::new(),
            services: Vec::new(),
            default: None,
            external: Vec::new(),
            extensions: Extensions::new(),
            error_renderer: DefaultError,
            case_insensitive: false,
        }
    }
}

impl<Err: ErrorRenderer> App<Identity, Filter<Err>, Err> {
    #[must_use]
    /// Create application builder with custom error renderer.
    pub fn with(err: Err) -> Self {
        App {
            middleware: Identity,
            filter: chain_factory(Filter::new()),
            state_factories: Vec::new(),
            services: Vec::new(),
            default: None,
            external: Vec::new(),
            extensions: Extensions::new(),
            error_renderer: err,
            case_insensitive: false,
        }
    }
}

impl<M, T, Err> App<M, T, Err>
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
    /// Set application level arbitrary state item.
    ///
    /// Application state stored with `App::state()` method is available
    /// via `HttpRequest::app_state()` method at runtime.
    ///
    /// `State<T>` extractor could be used to access stored state `T`.
    ///
    /// **Note**: http server accepts an application factory rather than
    /// an application instance. Http server constructs an application
    /// instance for each thread, thus application state must be constructed
    /// multiple times. If you want to share state between different
    /// threads, a shared object should be used, e.g. `Arc`.
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
    /// let app = App::new()
    ///     .state(MyState{ counter: Cell::new(0) })
    ///     .service(
    ///         web::resource("/index.html").route(web::get().to(index))
    ///     );
    /// ```
    pub fn state<U: 'static>(mut self, state: U) -> Self {
        self.extensions.insert(state);
        self
    }

    #[must_use]
    /// Set application state factory.
    ///
    /// This function is similar to `.state()` but it accepts state factory.
    /// State object get constructed asynchronously during application initialization.
    pub fn state_factory<F, D, E>(mut self, state: F) -> Self
    where
        F: AsyncFnOnce() -> Result<D, E> + 'static,
        D: 'static,
        E: fmt::Debug,
    {
        let state = Cell::new(Some(state));

        self.state_factories.push(Box::new(move |mut ext| {
            let mut state = state.take();

            Box::pin(async move {
                if let Some(state) = state.take() {
                    match state().await {
                        Err(e) => {
                            log::error!("Cannot construct state instance: {e:?}");
                            Err(())
                        }
                        Ok(st) => {
                            ext.insert(st);
                            Ok(ext)
                        }
                    }
                } else {
                    log::error!("Cannot construct state instance");
                    Err(())
                }
            })
        }));
        self
    }

    #[must_use]
    /// Run external configuration as part of the application building
    /// process.
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
    ///         .configure(config)  // <- register resources
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
        self.extensions.extend(cfg.state);
        self
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
    ///     let app = App::new()
    ///         .route("/test1", web::get().to(index))
    ///         .route("/test2", web::post().to(|| async { HttpResponse::MethodNotAllowed() }));
    /// }
    /// ```
    pub fn route(self, path: &str, mut route: Route<Err>) -> Self {
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
    pub fn service<F>(mut self, factory: F) -> Self
    where
        F: WebServiceFactory<Err> + 'static,
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
    ///     let app = App::new()
    ///         .service(
    ///             web::resource("/index.html").route(web::get().to(index)))
    ///         .default_service(
    ///             web::route().to(|| async { HttpResponse::NotFound() }));
    /// }
    /// ```
    ///
    /// It is also possible to use static files as default service.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpResponse};
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .service(
    ///             web::resource("/index.html").to(|| async { HttpResponse::Ok() }))
    ///         .default_service(
    ///             web::to(|| async { HttpResponse::NotFound() })
    ///         );
    /// }
    /// ```
    pub fn default_service<F, U>(mut self, f: F) -> Self
    where
        F: IntoServiceFactory<U, WebRequest<Err>, SharedCfg>,
        U: ServiceFactory<
                WebRequest<Err>,
                SharedCfg,
                Response = WebResponse,
                Error = Err::Container,
            > + 'static,
        U::InitError: fmt::Debug,
    {
        // create and configure default resource
        self.default = Some(Rc::new(boxed::factory(
            chain_factory(f)
                .map_init_err(|e| log::error!("Cannot construct default service: {e:?}")),
        )));

        self
    }

    #[must_use]
    /// Register an external resource.
    ///
    /// External resources are useful for URL generation purposes only
    /// and are never considered for matching at request time. Calls to
    /// `HttpRequest::url_for()` will work as expected.
    ///
    /// ```rust
    /// use ntex::web::{self, App, HttpRequest, HttpResponse, Error};
    ///
    /// async fn index(req: HttpRequest) -> Result<HttpResponse, Error> {
    ///     let url = req.url_for("youtube", &["asdlkjqme"])?;
    ///     assert_eq!(url.as_str(), "https://youtube.com/watch/asdlkjqme");
    ///     Ok(HttpResponse::Ok().into())
    /// }
    ///
    /// fn main() {
    ///     let app = App::new()
    ///         .service(web::resource("/index.html").route(
    ///             web::get().to(index)))
    ///         .external_resource("youtube", "https://youtube.com/watch/{video_id}");
    /// }
    /// ```
    pub fn external_resource<N, U>(mut self, name: N, url: U) -> Self
    where
        N: AsRef<str>,
        U: AsRef<str>,
    {
        let mut rdef = ResourceDef::new(url.as_ref());
        *rdef.name_mut() = name.as_ref().to_string();
        self.external.push(rdef);
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
    ///     let app = App::new()
    ///         .wrap(middleware::Logger::default())
    ///         .route("/index.html", web::get().to(index));
    /// }
    /// ```
    pub fn filter<S, U>(
        self,
        filter: U,
    ) -> App<
        M,
        impl ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
        Err,
    >
    where
        S: ServiceFactory<
                WebRequest<Err>,
                SharedCfg,
                Response = WebRequest<Err>,
                Error = Err::Container,
            >,
        U: IntoServiceFactory<S, WebRequest<Err>, SharedCfg>,
    {
        App {
            filter: self
                .filter
                .and_then(filter.into_factory().map_init_err(|_| ())),
            middleware: self.middleware,
            state_factories: self.state_factories,
            services: self.services,
            default: self.default,
            external: self.external,
            extensions: self.extensions,
            error_renderer: self.error_renderer,
            case_insensitive: self.case_insensitive,
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
    /// Notice that the keyword for registering middleware is `wrap`. As you
    /// register middleware using `wrap` in the App builder, imagine wrapping
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
    ///     let app = App::new()
    ///         .wrap(middleware::Logger::default())
    ///         .route("/index.html", web::get().to(index));
    /// }
    /// ```
    pub fn wrap<U>(self, mw: U) -> App<WebStack<M, U, Err>, T, Err> {
        App {
            middleware: WebStack::new(self.middleware, mw),
            filter: self.filter,
            state_factories: self.state_factories,
            services: self.services,
            default: self.default,
            external: self.external,
            extensions: self.extensions,
            error_renderer: self.error_renderer,
            case_insensitive: self.case_insensitive,
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

impl<M, F, Err> App<M, F, Err>
where
    M: Middleware<AppService<F::Service, Err>, SharedCfg> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    Err: ErrorRenderer,
{
    /// Construct service factory, suitable for `http::HttpService`.
    ///
    /// ```rust,no_run
    /// use ntex::{web, http, server};
    ///
    /// #[ntex::main]
    /// async fn main() -> std::io::Result<()> {
    ///     server::build().bind("http", "127.0.0.1:0", async |_|
    ///         http::HttpService::new(
    ///             web::App::new()
    ///                 .route("/index.html", web::get().to(|| async { "hello_world" }))
    ///                 .finish()
    ///         )
    ///     )?
    ///     .run()
    ///     .await
    /// }
    /// ```
    pub fn finish(
        self,
    ) -> impl ServiceFactory<
        Request,
        SharedCfg,
        Response = WebResponse,
        Error = Err::Container,
        InitError = (),
    > {
        IntoServiceFactory::<AppFactory<M, F, Err>, Request, SharedCfg>::into_factory(self)
    }
}

impl<M, F, Err> IntoServiceFactory<AppFactory<M, F, Err>, Request, SharedCfg>
    for App<M, F, Err>
where
    M: Middleware<AppService<F::Service, Err>, SharedCfg> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            WebRequest<Err>,
            SharedCfg,
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    Err: ErrorRenderer,
{
    fn into_factory(self) -> AppFactory<M, F, Err> {
        AppFactory {
            filter: self.filter,
            middleware: Rc::new(self.middleware),
            state_factories: Rc::new(self.state_factories),
            services: Rc::new(RefCell::new(self.services)),
            external: RefCell::new(self.external),
            default: self.default,
            extensions: RefCell::new(Some(self.extensions)),
            case_insensitive: self.case_insensitive,
        }
    }
}

pub struct Filter<Err>(PhantomData<Err>);

impl<Err: ErrorRenderer> Filter<Err> {
    pub(super) fn new() -> Self {
        Filter(PhantomData)
    }
}

impl<Err: ErrorRenderer> ServiceFactory<WebRequest<Err>, SharedCfg> for Filter<Err> {
    type Response = WebRequest<Err>;
    type Error = Err::Container;
    type InitError = ();
    type Service = Filter<Err>;

    async fn create(&self, _: SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(Filter(PhantomData))
    }
}

impl<Err: ErrorRenderer> Service<WebRequest<Err>> for Filter<Err> {
    type Response = WebRequest<Err>;
    type Error = Err::Container;

    async fn call(
        &self,
        req: WebRequest<Err>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<WebRequest<Err>, Err::Container> {
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Method, StatusCode, header, header::HeaderValue};
    use crate::web::test::{TestRequest, call_service, init_service, read_body};
    use crate::web::{self, HttpRequest, HttpResponse, middleware::DefaultHeaders};
    use crate::{service::fn_service, util::Ready};

    #[crate::rt_test]
    async fn test_default_resource() {
        let srv = App::new()
            .service(web::resource("/test").to(|| async { HttpResponse::Ok() }))
            .finish()
            .pipeline(SharedCfg::default())
            .await
            .unwrap();
        let req = TestRequest::with_uri("/test").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/blah").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let srv = App::new()
            .service(web::resource("/test").to(|| async { HttpResponse::Ok() }))
            .service(
                web::resource("/test2")
                    .default_service(|r: WebRequest<DefaultError>| async move {
                        Ok(r.into_response(HttpResponse::Created()))
                    })
                    .route(web::get().to(|| async { HttpResponse::Ok() })),
            )
            .default_service(|r: WebRequest<DefaultError>| async move {
                Ok(r.into_response(HttpResponse::MethodNotAllowed()))
            })
            .finish()
            .pipeline(SharedCfg::default())
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
    async fn test_state_factory() {
        let srv = init_service(
            App::new()
                .state_factory(|| async { Ok::<_, ()>(10usize) })
                .service(
                    web::resource("/")
                        .to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
                ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let srv = init_service(
            App::new()
                .state_factory(|| async { Ok::<_, ()>(10u32) })
                .service(
                    web::resource("/")
                        .to(|_: web::types::State<usize>| async { HttpResponse::Ok() }),
                ),
        )
        .await;
        let req = TestRequest::default().to_request();
        let res = srv.call(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[crate::rt_test]
    async fn test_extension() {
        let srv = init_service(
            App::new()
                .state(10usize)
                .filter(fn_service(move |req: WebRequest<_>| {
                    assert_eq!(*req.app_state::<usize>().unwrap(), 10);
                    Ready::Ok(req)
                }))
                .service(web::resource("/").to(|req: HttpRequest| async move {
                    assert_eq!(*req.app_state::<usize>().unwrap(), 10);
                    HttpResponse::Ok()
                })),
        )
        .await;
        let req = TestRequest::default().to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[crate::rt_test]
    async fn test_filter() {
        let filter = Rc::new(std::cell::Cell::new(false));
        let filter2 = filter.clone();
        let srv = init_service(
            App::new()
                .filter(fn_service(move |req: WebRequest<_>| {
                    filter2.set(true);
                    Ready::Ok(req)
                }))
                .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
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
                .wrap(
                    DefaultHeaders::new()
                        .header(header::CONTENT_TYPE, HeaderValue::from_static("0001")),
                )
                .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
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
                .route("/test", web::get().to(|| async { HttpResponse::Ok() }))
                .wrap(
                    DefaultHeaders::new()
                        .header(header::CONTENT_TYPE, HeaderValue::from_static("0001")),
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
    async fn test_case_insensitive_router() {
        let srv = init_service(
            App::new()
                .case_insensitive_routing()
                .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/Test").to_request();
        let resp = call_service(&srv, req).await;
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
                    web::get().to(|req: HttpRequest| async move {
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
