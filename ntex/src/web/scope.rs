use std::cell::RefCell;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_router::{ResourceDef, ResourceInfo, Router};
use futures::future::{ok, Either, Future, LocalBoxFuture, Ready};

use crate::http::{Extensions, Response};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::{
    apply, apply_fn_factory, IntoServiceFactory, Service, ServiceFactory, Transform,
};

use super::config::ServiceConfig;
use super::data::Data;
use super::dev::{AppService, HttpServiceFactory};
use super::error::WebError;
use super::guard::Guard;
use super::resource::Resource;
use super::rmap::ResourceMap;
use super::route::Route;
use super::service::{
    AppServiceFactory, ServiceFactoryWrapper, WebRequest, WebResponse,
};

type Guards = Vec<Box<dyn Guard>>;
type HttpService<Err> = BoxService<WebRequest, WebResponse, WebError<Err>>;
type HttpNewService<Err> =
    BoxServiceFactory<(), WebRequest, WebResponse, WebError<Err>, ()>;
type BoxedResponse<Err> = LocalBoxFuture<'static, Result<WebResponse, WebError<Err>>>;

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
pub struct Scope<Err, T = ScopeEndpoint<Err>> {
    endpoint: T,
    rdef: String,
    data: Option<Extensions>,
    services: Vec<Box<dyn AppServiceFactory<Err>>>,
    guards: Vec<Box<dyn Guard>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
    external: Vec<ResourceDef>,
    factory_ref: Rc<RefCell<Option<ScopeFactory<Err>>>>,
}

impl<Err: 'static> Scope<Err> {
    /// Create a new scope
    pub fn new(path: &str) -> Scope<Err> {
        let fref = Rc::new(RefCell::new(None));
        Scope {
            endpoint: ScopeEndpoint::new(fref.clone()),
            rdef: path.to_string(),
            data: None,
            guards: Vec::new(),
            services: Vec::new(),
            default: Rc::new(RefCell::new(None)),
            external: Vec::new(),
            factory_ref: fref,
        }
    }
}

impl<Err, T> Scope<Err, T>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest,
        Response = WebResponse,
        Error = WebError<Err>,
        InitError = (),
    >,
    Err: 'static,
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

    /// Set or override application data. Application data could be accessed
    /// by using `Data<T>` extractor where `T` is data type.
    ///
    /// ```rust
    /// use std::cell::Cell;
    /// use ntex::web::{self, App, HttpResponse, Responder};
    ///
    /// struct MyData {
    ///     counter: Cell<usize>,
    /// }
    ///
    /// async fn index(data: web::Data<MyData>) -> impl Responder {
    ///     data.counter.set(data.counter.get() + 1);
    ///     HttpResponse::Ok()
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::scope("/app")
    ///             .data(MyData{ counter: Cell::new(0) })
    ///             .service(
    ///                 web::resource("/index.html").route(
    ///                     web::get().to(index)))
    ///     );
    /// }
    /// ```
    pub fn data<U: 'static>(self, data: U) -> Self {
        self.app_data(Data::new(data))
    }

    /// Set or override application data.
    ///
    /// This method overrides data stored with [`App::app_data()`](#method.app_data)
    pub fn app_data<U: 'static>(mut self, data: U) -> Self {
        if self.data.is_none() {
            self.data = Some(Extensions::new());
        }
        self.data.as_mut().unwrap().insert(data);
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

        if !cfg.data.is_empty() {
            let mut data = self.data.unwrap_or_else(Extensions::new);

            for value in cfg.data.iter() {
                value.create(&mut data);
            }

            self.data = Some(data);
        }
        self
    }

    /// Register http service.
    ///
    /// This is similar to `App's` service registration.
    ///
    /// Actix web provides several services implementations:
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
        F: HttpServiceFactory<Err> + 'static,
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
    pub fn default_service<F, U>(mut self, f: F) -> Self
    where
        F: IntoServiceFactory<U>,
        U: ServiceFactory<
                Config = (),
                Request = WebRequest,
                Response = WebResponse,
                Error = WebError<Err>,
            > + 'static,
        U::InitError: fmt::Debug,
    {
        // create and configure default resource
        self.default = Rc::new(RefCell::new(Some(Rc::new(boxed::factory(
            f.into_factory().map_init_err(|e| {
                log::error!("Can not construct default service: {:?}", e)
            }),
        )))));

        self
    }

    /// Registers middleware, in the form of a middleware component (type),
    /// that runs during inbound processing in the request
    /// lifecycle (request -> response), modifying request as
    /// necessary, across all requests managed by the *Scope*.  Scope-level
    /// middleware is more limited in what it can modify, relative to Route or
    /// Application level middleware, in that Scope-level middleware can not modify
    /// WebResponse.
    ///
    /// Use middleware when you need to read or modify *every* request in some way.
    pub fn wrap<M>(
        self,
        mw: M,
    ) -> Scope<
        Err,
        impl ServiceFactory<
            Config = (),
            Request = WebRequest,
            Response = WebResponse,
            Error = WebError<Err>,
            InitError = (),
        >,
    >
    where
        M: Transform<
            T::Service,
            Request = WebRequest,
            Response = WebResponse,
            Error = WebError<Err>,
            InitError = (),
        >,
    {
        Scope {
            endpoint: apply(mw, self.endpoint),
            rdef: self.rdef,
            data: self.data,
            guards: self.guards,
            services: self.services,
            default: self.default,
            external: self.external,
            factory_ref: self.factory_ref,
        }
    }

    /// Registers middleware, in the form of a closure, that runs during inbound
    /// processing in the request lifecycle (request -> response), modifying
    /// request as necessary, across all requests managed by the *Scope*.
    /// Scope-level middleware is more limited in what it can modify, relative
    /// to Route or Application level middleware, in that Scope-level middleware
    /// can not modify WebResponse.
    ///
    /// ```rust
    /// use ntex::service::Service;
    /// use ntex::web;
    /// use ntex::http::header::{CONTENT_TYPE, HeaderValue};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = web::App::new().service(
    ///         web::scope("/app")
    ///             .wrap_fn(|req, srv| {
    ///                 let fut = srv.call(req);
    ///                 async {
    ///                     let mut res = fut.await?;
    ///                     res.headers_mut().insert(
    ///                        CONTENT_TYPE, HeaderValue::from_static("text/plain"),
    ///                     );
    ///                     Ok(res)
    ///                 }
    ///             })
    ///             .route("/index.html", web::get().to(index)));
    /// }
    /// ```
    pub fn wrap_fn<F, R>(
        self,
        mw: F,
    ) -> Scope<
        Err,
        impl ServiceFactory<
            Config = (),
            Request = WebRequest,
            Response = WebResponse,
            Error = WebError<Err>,
            InitError = (),
        >,
    >
    where
        F: FnMut(WebRequest, &mut T::Service) -> R + Clone,
        R: Future<Output = Result<WebResponse, WebError<Err>>>,
    {
        Scope {
            endpoint: apply_fn_factory(self.endpoint, mw),
            rdef: self.rdef,
            data: self.data,
            guards: self.guards,
            services: self.services,
            default: self.default,
            external: self.external,
            factory_ref: self.factory_ref,
        }
    }
}

impl<Err, T> HttpServiceFactory<Err> for Scope<Err, T>
where
    T: ServiceFactory<
            Config = (),
            Request = WebRequest,
            Response = WebResponse,
            Error = WebError<Err>,
            InitError = (),
        > + 'static,
    Err: 'static,
{
    fn register(mut self, config: &mut AppService<Err>) {
        // update default resource if needed
        if self.default.borrow().is_none() {
            *self.default.borrow_mut() = Some(config.default_service());
        }

        // register nested services
        let mut cfg = config.clone_config();
        self.services
            .into_iter()
            .for_each(|mut srv| srv.register(&mut cfg));

        let mut rmap = ResourceMap::new(ResourceDef::root_prefix(&self.rdef));

        // external resources
        for mut rdef in std::mem::replace(&mut self.external, Vec::new()) {
            rmap.add(&mut rdef, None);
        }

        // custom app data storage
        if let Some(ref mut ext) = self.data {
            config.set_service_data(ext);
        }

        // complete scope pipeline creation
        *self.factory_ref.borrow_mut() = Some(ScopeFactory {
            data: self.data.take().map(Rc::new),
            default: self.default.clone(),
            services: Rc::new(
                cfg.into_services()
                    .1
                    .into_iter()
                    .map(|(mut rdef, srv, guards, nested)| {
                        rmap.add(&mut rdef, nested);
                        (rdef, srv, RefCell::new(guards))
                    })
                    .collect(),
            ),
        });

        // get guards
        let guards = if self.guards.is_empty() {
            None
        } else {
            Some(self.guards)
        };

        // register final service
        config.register_service(
            ResourceDef::root_prefix(&self.rdef),
            guards,
            self.endpoint,
            Some(Rc::new(rmap)),
        )
    }
}

pub struct ScopeFactory<Err> {
    data: Option<Rc<Extensions>>,
    services: Rc<Vec<(ResourceDef, HttpNewService<Err>, RefCell<Option<Guards>>)>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
}

impl<Err: 'static> ServiceFactory for ScopeFactory<Err> {
    type Config = ();
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type InitError = ();
    type Service = ScopeService<Err>;
    type Future = ScopeFactoryResponse<Err>;

    fn new_service(&self, _: ()) -> Self::Future {
        let default_fut = if let Some(ref default) = *self.default.borrow() {
            Some(default.new_service(()))
        } else {
            None
        };

        ScopeFactoryResponse {
            fut: self
                .services
                .iter()
                .map(|(path, service, guards)| {
                    CreateScopeServiceItem::Future(
                        Some(path.clone()),
                        guards.borrow_mut().take(),
                        service.new_service(()),
                    )
                })
                .collect(),
            default: None,
            data: self.data.clone(),
            default_fut,
        }
    }
}

/// Create scope service
#[doc(hidden)]
#[pin_project::pin_project]
pub struct ScopeFactoryResponse<Err> {
    fut: Vec<CreateScopeServiceItem<Err>>,
    data: Option<Rc<Extensions>>,
    default: Option<HttpService<Err>>,
    default_fut: Option<LocalBoxFuture<'static, Result<HttpService<Err>, ()>>>,
}

type HttpServiceFut<Err> = LocalBoxFuture<'static, Result<HttpService<Err>, ()>>;

enum CreateScopeServiceItem<Err> {
    Future(Option<ResourceDef>, Option<Guards>, HttpServiceFut<Err>),
    Service(ResourceDef, Option<Guards>, HttpService<Err>),
}

impl<Err> Future for ScopeFactoryResponse<Err> {
    type Output = Result<ScopeService<Err>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut done = true;

        if let Some(ref mut fut) = self.default_fut {
            match Pin::new(fut).poll(cx)? {
                Poll::Ready(default) => self.default = Some(default),
                Poll::Pending => done = false,
            }
        }

        // poll http services
        for item in &mut self.fut {
            let res = match item {
                CreateScopeServiceItem::Future(
                    ref mut path,
                    ref mut guards,
                    ref mut fut,
                ) => match Pin::new(fut).poll(cx)? {
                    Poll::Ready(service) => {
                        Some((path.take().unwrap(), guards.take(), service))
                    }
                    Poll::Pending => {
                        done = false;
                        None
                    }
                },
                CreateScopeServiceItem::Service(_, _, _) => continue,
            };

            if let Some((path, guards, service)) = res {
                *item = CreateScopeServiceItem::Service(path, guards, service);
            }
        }

        if done {
            let router = self
                .fut
                .drain(..)
                .fold(Router::build(), |mut router, item| {
                    match item {
                        CreateScopeServiceItem::Service(path, guards, service) => {
                            router.rdef(path, service).2 = guards;
                        }
                        CreateScopeServiceItem::Future(_, _, _) => unreachable!(),
                    }
                    router
                });
            Poll::Ready(Ok(ScopeService {
                data: self.data.clone(),
                router: router.finish(),
                default: self.default.take(),
                _ready: None,
            }))
        } else {
            Poll::Pending
        }
    }
}

pub struct ScopeService<Err> {
    data: Option<Rc<Extensions>>,
    router: Router<HttpService<Err>, Vec<Box<dyn Guard>>>,
    default: Option<HttpService<Err>>,
    _ready: Option<(WebRequest, ResourceInfo)>,
}

impl<Err> Service for ScopeService<Err> {
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type Future = Either<BoxedResponse<Err>, Ready<Result<Self::Response, Self::Error>>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: WebRequest) -> Self::Future {
        let res = self.router.recognize_mut_checked(&mut req, |req, guards| {
            if let Some(ref guards) = guards {
                for f in guards {
                    if !f.check(req.head()) {
                        return false;
                    }
                }
            }
            true
        });

        if let Some((srv, _info)) = res {
            if let Some(ref data) = self.data {
                req.set_data_container(data.clone());
            }
            Either::Left(srv.call(req))
        } else if let Some(ref mut default) = self.default {
            Either::Left(default.call(req))
        } else {
            let req = req.into_parts().0;
            Either::Right(ok(WebResponse::new(req, Response::NotFound().finish())))
        }
    }
}

#[doc(hidden)]
pub struct ScopeEndpoint<Err> {
    factory: Rc<RefCell<Option<ScopeFactory<Err>>>>,
}

impl<Err: 'static> ScopeEndpoint<Err> {
    fn new(factory: Rc<RefCell<Option<ScopeFactory<Err>>>>) -> Self {
        ScopeEndpoint { factory }
    }
}

impl<Err: 'static> ServiceFactory for ScopeEndpoint<Err> {
    type Config = ();
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type InitError = ();
    type Service = ScopeService<Err>;
    type Future = ScopeFactoryResponse<Err>;

    fn new_service(&self, _: ()) -> Self::Future {
        self.factory.borrow_mut().as_mut().unwrap().new_service(())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::future::{ok, ready};

    use crate::http::body::{Body, ResponseBody};
    use crate::http::header::{HeaderValue, CONTENT_TYPE};
    use crate::http::{Method, StatusCode};
    use crate::service::Service;
    use crate::web::middleware::DefaultHeaders;
    use crate::web::service::WebRequest;
    use crate::web::test::{call_service, init_service, read_body, TestRequest};
    use crate::web::{self, guard, App, HttpRequest, HttpResponse};

    #[actix_rt::test]
    async fn test_scope() {
        let mut srv =
            init_service(App::new().service(
                web::scope("/app").service(
                    web::resource("/path1").to(|| async { HttpResponse::Ok() }),
                ),
            ))
            .await;

        let req = TestRequest::with_uri("/app/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_scope_root() {
        let mut srv = init_service(
            App::new().service(
                web::scope("/app")
                    .service(web::resource("").to(|| async { HttpResponse::Ok() }))
                    .service(
                        web::resource("/").to(|| async { HttpResponse::Created() }),
                    ),
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

    #[actix_rt::test]
    async fn test_scope_root2() {
        let mut srv = init_service(
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

    #[actix_rt::test]
    async fn test_scope_root3() {
        let mut srv = init_service(
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
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_scope_route() {
        let mut srv = init_service(
            App::new().service(
                web::scope("app")
                    .route("/path1", web::get().to(|| async { HttpResponse::Ok() }))
                    .route("/path1", web::delete().to(|| async { HttpResponse::Ok() })),
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
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_scope_route_without_leading_slash() {
        let mut srv = init_service(
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

    #[actix_rt::test]
    async fn test_scope_guard() {
        let mut srv =
            init_service(App::new().service(
                web::scope("/app").guard(guard::Get()).service(
                    web::resource("/path1").to(|| async { HttpResponse::Ok() }),
                ),
            ))
            .await;

        let req = TestRequest::with_uri("/app/path1")
            .method(Method::POST)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let req = TestRequest::with_uri("/app/path1")
            .method(Method::GET)
            .to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_scope_variable_segment() {
        let mut srv =
            init_service(App::new().service(web::scope("/ab-{project}").service(
                web::resource("/path1").to(|r: HttpRequest| async move {
                    HttpResponse::Ok()
                        .body(format!("project: {}", &r.match_info()["project"]))
                }),
            )))
            .await;

        let req = TestRequest::with_uri("/ab-project1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        match resp.response().body() {
            ResponseBody::Body(Body::Bytes(ref b)) => {
                let bytes: Bytes = b.clone().into();
                assert_eq!(bytes, Bytes::from_static(b"project: project1"));
            }
            _ => panic!(),
        }

        let req = TestRequest::with_uri("/aa-project1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_nested_scope() {
        let mut srv = init_service(App::new().service(web::scope("/app").service(
            web::scope("/t1").service(
                web::resource("/path1").to(|| async { HttpResponse::Created() }),
            ),
        )))
        .await;

        let req = TestRequest::with_uri("/app/t1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[actix_rt::test]
    async fn test_nested_scope_no_slash() {
        let mut srv = init_service(App::new().service(web::scope("/app").service(
            web::scope("t1").service(
                web::resource("/path1").to(|| async { HttpResponse::Created() }),
            ),
        )))
        .await;

        let req = TestRequest::with_uri("/app/t1/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[actix_rt::test]
    async fn test_nested_scope_root() {
        let mut srv = init_service(
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

    #[actix_rt::test]
    async fn test_nested_scope_filter() {
        let mut srv =
            init_service(App::new().service(web::scope("/app").service(
                web::scope("/t1").guard(guard::Get()).service(
                    web::resource("/path1").to(|| async { HttpResponse::Ok() }),
                ),
            )))
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

    #[actix_rt::test]
    async fn test_nested_scope_with_variable_segment() {
        let mut srv = init_service(App::new().service(web::scope("/app").service(
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

        match resp.response().body() {
            ResponseBody::Body(Body::Bytes(ref b)) => {
                let bytes: Bytes = b.clone().into();
                assert_eq!(bytes, Bytes::from_static(b"project: project_1"));
            }
            _ => panic!(),
        }
    }

    #[actix_rt::test]
    async fn test_nested2_scope_with_variable_segment() {
        let mut srv = init_service(App::new().service(web::scope("/app").service(
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

        match resp.response().body() {
            ResponseBody::Body(Body::Bytes(ref b)) => {
                let bytes: Bytes = b.clone().into();
                assert_eq!(bytes, Bytes::from_static(b"project: test - 1"));
            }
            _ => panic!(),
        }

        let req = TestRequest::with_uri("/app/test/1/path2").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[actix_rt::test]
    async fn test_default_resource() {
        let mut srv = init_service(
            App::new().service(
                web::scope("/app")
                    .service(web::resource("/path1").to(|| async { HttpResponse::Ok() }))
                    .default_service(|r: WebRequest| {
                        ok(r.into_response(HttpResponse::BadRequest()))
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

    #[actix_rt::test]
    async fn test_default_resource_propagation() {
        let mut srv = init_service(
            App::new()
                .service(web::scope("/app1").default_service(
                    web::resource("").to(|| async { HttpResponse::BadRequest() }),
                ))
                .service(web::scope("/app2"))
                .default_service(|r: WebRequest| {
                    ok(r.into_response(HttpResponse::MethodNotAllowed()))
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

    #[actix_rt::test]
    async fn test_middleware() {
        let mut srv = init_service(
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
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[actix_rt::test]
    async fn test_middleware_fn() {
        let mut srv = init_service(
            App::new().service(
                web::scope("app")
                    .wrap_fn(|req, srv| {
                        let fut = srv.call(req);
                        async move {
                            let mut res = fut.await?;
                            res.headers_mut()
                                .insert(CONTENT_TYPE, HeaderValue::from_static("0001"));
                            Ok(res)
                        }
                    })
                    .route("/test", web::get().to(|| async { HttpResponse::Ok() })),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/app/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[actix_rt::test]
    async fn test_override_data() {
        let mut srv = init_service(App::new().data(1usize).service(
            web::scope("app").data(10usize).route(
                "/t",
                web::get().to(|data: web::Data<usize>| {
                    assert_eq!(**data, 10);
                    let _ = data.clone();
                    ready(HttpResponse::Ok())
                }),
            ),
        ))
        .await;

        let req = TestRequest::with_uri("/app/t").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_override_app_data() {
        let mut srv = init_service(App::new().app_data(web::Data::new(1usize)).service(
            web::scope("app").app_data(web::Data::new(10usize)).route(
                "/t",
                web::get().to(|data: web::Data<usize>| {
                    assert_eq!(**data, 10);
                    let _ = data.clone();
                    ready(HttpResponse::Ok())
                }),
            ),
        ))
        .await;

        let req = TestRequest::with_uri("/app/t").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_scope_config() {
        let mut srv =
            init_service(App::new().service(web::scope("/app").configure(|s| {
                s.route("/path1", web::get().to(|| async { HttpResponse::Ok() }));
            })))
            .await;

        let req = TestRequest::with_uri("/app/path1").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_scope_config_2() {
        let mut srv =
            init_service(App::new().service(web::scope("/app").configure(|s| {
                s.service(web::scope("/v1").configure(|s| {
                    s.route("/", web::get().to(|| async { HttpResponse::Ok() }));
                }));
            })))
            .await;

        let req = TestRequest::with_uri("/app/v1/").to_request();
        let resp = srv.call(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[actix_rt::test]
    async fn test_url_for_external() {
        let mut srv =
            init_service(App::new().service(web::scope("/app").configure(|s| {
                s.service(web::scope("/v1").configure(|s| {
                    s.external_resource(
                        "youtube",
                        "https://youtube.com/watch/{video_id}",
                    );
                    s.route(
                        "/",
                        web::get().to(|req: HttpRequest| async move {
                            HttpResponse::Ok().body(format!(
                                "{}",
                                req.url_for("youtube", &["xxxxxx"]).unwrap().as_str()
                            ))
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

    #[actix_rt::test]
    async fn test_url_for_nested() {
        let mut srv = init_service(App::new().service(web::scope("/a").service(
            web::scope("/b").service(web::resource("/c/{stuff}").name("c").route(
                web::get().to(|req: HttpRequest| async move {
                    HttpResponse::Ok()
                        .body(format!("{}", req.url_for("c", &["12345"]).unwrap()))
                }),
            )),
        )))
        .await;

        let req = TestRequest::with_uri("/a/b/c/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = read_body(resp).await;
        assert_eq!(
            body,
            Bytes::from_static(b"http://localhost:8080/a/b/c/12345")
        );
    }
}
