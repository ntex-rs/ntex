use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::{fmt, mem};

use futures::future::{ok, Either, LocalBoxFuture, Ready};

use crate::http::{Extensions, Response};
use crate::router::{IntoPattern, ResourceDef};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::{
    apply, apply_fn_factory, IntoServiceFactory, Service, ServiceFactory, Transform,
};

use super::dev::{insert_slesh, WebServiceConfig, WebServiceFactory};
use super::error::ErrorRenderer;
use super::extract::FromRequest;
use super::guard::Guard;
use super::handler::Handler;
use super::request::WebRequest;
use super::responder::Responder;
use super::response::WebResponse;
use super::route::{Route, RouteService};
use super::types::Data;

type HttpService<Err: ErrorRenderer> =
    BoxService<WebRequest<Err>, WebResponse, Err::Container>;
type HttpNewService<Err: ErrorRenderer> =
    BoxServiceFactory<(), WebRequest<Err>, WebResponse, Err::Container, ()>;

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
pub struct Resource<Err: ErrorRenderer, T = ResourceEndpoint<Err>> {
    endpoint: T,
    rdef: Vec<String>,
    name: Option<String>,
    routes: Vec<Route<Err>>,
    data: Option<Extensions>,
    guards: Vec<Box<dyn Guard>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
    factory_ref: Rc<RefCell<Option<ResourceFactory<Err>>>>,
}

impl<Err: ErrorRenderer> Resource<Err> {
    pub fn new<T: IntoPattern>(path: T) -> Resource<Err> {
        let fref = Rc::new(RefCell::new(None));

        Resource {
            routes: Vec::new(),
            rdef: path.patterns(),
            name: None,
            endpoint: ResourceEndpoint::new(fref.clone()),
            factory_ref: fref,
            guards: Vec::new(),
            data: None,
            default: Rc::new(RefCell::new(None)),
        }
    }
}

impl<Err, T> Resource<Err, T>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
        InitError = (),
    >,
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
    ///              .route(web::get().to(get_handler))
    ///              .route(web::post().to(post_handler))
    ///              .route(web::delete().to(delete_handler))
    ///     );
    /// }
    /// # async fn get_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// # async fn post_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// # async fn delete_handler() -> web::HttpResponseBuilder { web::HttpResponse::Ok() }
    /// ```
    pub fn route(mut self, route: Route<Err>) -> Self {
        self.routes.push(route);
        self
    }

    /// Provide resource specific data. This method allows to add extractor
    /// configuration or specific state available via `Data<T>` extractor.
    /// Provided data is available for all routes registered for the current resource.
    /// Resource data overrides data registered by `App::data()` method.
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
    ///           .app_data(web::types::PayloadConfig::new(4096))
    ///           .route(
    ///               web::get()
    ///                  // register handler
    ///                  .to(index)
    ///           ));
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
    pub fn to<F, Args>(mut self, handler: F) -> Self
    where
        F: Handler<Args, Err>,
        Args: FromRequest<Err> + 'static,
        Args::Error: Into<Err::Container>,
        <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    {
        self.routes.push(Route::new().to(handler));
        self
    }

    /// Register a resource middleware.
    ///
    /// This is similar to `App's` middlewares, but middleware get invoked on resource level.
    /// Resource level middlewares are not allowed to change response
    /// type (i.e modify response's body).
    ///
    /// **Note**: middlewares get called in opposite order of middlewares registration.
    pub fn wrap<M>(
        self,
        mw: M,
    ) -> Resource<
        Err,
        impl ServiceFactory<
            Config = (),
            Request = WebRequest<Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        >,
    >
    where
        M: Transform<
            T::Service,
            Request = WebRequest<Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        >,
    {
        Resource {
            endpoint: apply(mw, self.endpoint),
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
            data: self.data,
            factory_ref: self.factory_ref,
        }
    }

    /// Register a resource middleware function.
    ///
    /// This function accepts instance of `WebRequest` type and
    /// mutable reference to the next middleware in chain.
    ///
    /// This is similar to `App's` middlewares, but middleware get invoked on resource level.
    /// Resource level middlewares are not allowed to change response
    /// type (i.e modify response's body).
    ///
    /// ```rust
    /// use ntex::service::Service;
    /// use ntex::web::{self, App};
    /// use ntex::http::header::{CONTENT_TYPE, HeaderValue};
    ///
    /// async fn index() -> &'static str {
    ///     "Welcome!"
    /// }
    ///
    /// fn main() {
    ///     let app = App::new().service(
    ///         web::resource("/index.html")
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
    ///             .route(web::get().to(index)));
    /// }
    /// ```
    pub fn wrap_fn<F, R>(
        self,
        mw: F,
    ) -> Resource<
        Err,
        impl ServiceFactory<
            Config = (),
            Request = WebRequest<Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        >,
    >
    where
        F: Fn(WebRequest<Err>, &T::Service) -> R + Clone,
        R: Future<Output = Result<WebResponse, Err::Container>>,
    {
        Resource {
            endpoint: apply_fn_factory(self.endpoint, mw),
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
            data: self.data,
            factory_ref: self.factory_ref,
        }
    }

    /// Default service to be used if no matching route could be found.
    /// By default *405* response get returned. Resource does not use
    /// default handler from `App` or `Scope`.
    pub fn default_service<F, U>(mut self, f: F) -> Self
    where
        F: IntoServiceFactory<U>,
        U: ServiceFactory<
                Config = (),
                Request = WebRequest<Err>,
                Response = WebResponse,
                Error = Err::Container,
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
}

impl<Err, T> WebServiceFactory<Err> for Resource<Err, T>
where
    T: ServiceFactory<
            Config = (),
            Request = WebRequest<Err>,
            Response = WebResponse,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    fn register(mut self, config: &mut WebServiceConfig<Err>) {
        let guards = if self.guards.is_empty() {
            None
        } else {
            Some(std::mem::replace(&mut self.guards, Vec::new()))
        };
        let mut rdef = if config.is_root() || !self.rdef.is_empty() {
            ResourceDef::new(insert_slesh(self.rdef.clone()))
        } else {
            ResourceDef::new(self.rdef.clone())
        };
        if let Some(ref name) = self.name {
            *rdef.name_mut() = name.clone();
        }
        // custom app data storage
        if let Some(ref mut ext) = self.data {
            config.set_service_data(ext);
        }
        config.register_service(rdef, guards, self, None)
    }
}

impl<Err, T> IntoServiceFactory<T> for Resource<Err, T>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest<Err>,
        Response = WebResponse,
        Error = Err::Container,
        InitError = (),
    >,
    Err: ErrorRenderer,
{
    fn into_factory(self) -> T {
        *self.factory_ref.borrow_mut() = Some(ResourceFactory {
            routes: self.routes,
            data: self.data.map(Rc::new),
            default: self.default,
        });

        self.endpoint
    }
}

struct ResourceFactory<Err: ErrorRenderer> {
    routes: Vec<Route<Err>>,
    data: Option<Rc<Extensions>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
}

impl<Err: ErrorRenderer> ServiceFactory for ResourceFactory<Err> {
    type Config = ();
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = ResourceService<Err>;
    type Future = CreateResourceService<Err>;

    fn new_service(&self, _: ()) -> Self::Future {
        let default_fut = if let Some(ref default) = *self.default.borrow() {
            Some(default.new_service(()))
        } else {
            None
        };

        CreateResourceService {
            routes: self.routes.iter().map(|route| route.service()).collect(),
            data: self.data.clone(),
            default: None,
            default_fut,
        }
    }
}

pub struct CreateResourceService<Err: ErrorRenderer> {
    routes: Vec<RouteService<Err>>,
    data: Option<Rc<Extensions>>,
    default: Option<HttpService<Err>>,
    default_fut: Option<LocalBoxFuture<'static, Result<HttpService<Err>, ()>>>,
}

impl<Err: ErrorRenderer> Future for CreateResourceService<Err> {
    type Output = Result<ResourceService<Err>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut done = true;

        if let Some(ref mut fut) = self.default_fut {
            match Pin::new(fut).poll(cx)? {
                Poll::Ready(default) => self.default = Some(default),
                Poll::Pending => done = false,
            }
        }

        if done {
            Poll::Ready(Ok(ResourceService {
                routes: mem::take(&mut self.routes),
                data: self.data.clone(),
                default: self.default.take(),
            }))
        } else {
            Poll::Pending
        }
    }
}

pub struct ResourceService<Err: ErrorRenderer> {
    routes: Vec<RouteService<Err>>,
    data: Option<Rc<Extensions>>,
    default: Option<HttpService<Err>>,
}

impl<Err: ErrorRenderer> Service for ResourceService<Err> {
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = Either<
        Ready<Result<WebResponse, Err::Container>>,
        LocalBoxFuture<'static, Result<WebResponse, Err::Container>>,
    >;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&self, mut req: WebRequest<Err>) -> Self::Future {
        for route in self.routes.iter() {
            if route.check(&mut req) {
                if let Some(ref data) = self.data {
                    req.set_data_container(data.clone());
                }
                return Either::Right(route.call(req));
            }
        }
        if let Some(ref default) = self.default {
            Either::Right(default.call(req))
        } else {
            let req = req.into_parts().0;
            Either::Left(ok(WebResponse::new(
                req,
                Response::MethodNotAllowed().finish(),
            )))
        }
    }
}

#[doc(hidden)]
pub struct ResourceEndpoint<Err: ErrorRenderer> {
    factory: Rc<RefCell<Option<ResourceFactory<Err>>>>,
}

impl<Err: ErrorRenderer> ResourceEndpoint<Err> {
    fn new(factory: Rc<RefCell<Option<ResourceFactory<Err>>>>) -> Self {
        ResourceEndpoint { factory }
    }
}

impl<Err: ErrorRenderer> ServiceFactory for ResourceEndpoint<Err> {
    type Config = ();
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = ResourceService<Err>;
    type Future = CreateResourceService<Err>;

    fn new_service(&self, _: ()) -> Self::Future {
        self.factory.borrow_mut().as_mut().unwrap().new_service(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::future::{ok, ready};

    use crate::http::header::{self, HeaderValue};
    use crate::http::{Method, StatusCode};
    use crate::rt::time::delay_for;
    use crate::web::middleware::DefaultHeaders;
    use crate::web::request::WebRequest;
    use crate::web::test::{call_service, init_service, TestRequest};
    use crate::web::{self, guard, App, DefaultError, HttpResponse};
    use crate::Service;

    #[ntex_rt::test]
    async fn test_middleware() {
        let mut srv =
            init_service(
                App::new().service(
                    web::resource("/test")
                        .name("test")
                        .wrap(DefaultHeaders::new().header(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("0001"),
                        ))
                        .route(web::get().to(|| async { HttpResponse::Ok() })),
                ),
            )
            .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[ntex_rt::test]
    async fn test_middleware_fn() {
        let mut srv = init_service(
            App::new().service(
                web::resource("/test")
                    .wrap_fn(|req, srv| {
                        let fut = srv.call(req);
                        async {
                            fut.await.map(|mut res| {
                                res.headers_mut().insert(
                                    header::CONTENT_TYPE,
                                    HeaderValue::from_static("0001"),
                                );
                                res
                            })
                        }
                    })
                    .route(web::get().to(|| async { HttpResponse::Ok() })),
            ),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[ntex_rt::test]
    async fn test_to() {
        let mut srv =
            init_service(App::new().service(web::resource("/test").to(|| async {
                delay_for(Duration::from_millis(100)).await;
                HttpResponse::Ok()
            })))
            .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex_rt::test]
    async fn test_pattern() {
        let mut srv = init_service(App::new().service(
            web::resource(["/test", "/test2"]).to(|| async { HttpResponse::Ok() }),
        ))
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let req = TestRequest::with_uri("/test2").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[ntex_rt::test]
    async fn test_default_resource() {
        let mut srv = init_service(
            App::new()
                .service(
                    web::resource("/test")
                        .route(web::get().to(|| async { HttpResponse::Ok() })),
                )
                .default_service(|r: WebRequest<DefaultError>| {
                    ok(r.into_response(HttpResponse::BadRequest()))
                }),
        )
        .await;
        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test")
            .method(Method::POST)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let mut srv = init_service(
            App::new().service(
                web::resource("/test")
                    .route(web::get().to(|| async { HttpResponse::Ok() }))
                    .default_service(|r: WebRequest<DefaultError>| {
                        ok(r.into_response(HttpResponse::BadRequest()))
                    }),
            ),
        )
        .await;

        let req = TestRequest::with_uri("/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test")
            .method(Method::POST)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[ntex_rt::test]
    async fn test_resource_guards() {
        let mut srv = init_service(
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
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test/it")
            .method(Method::PUT)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = TestRequest::with_uri("/test/it")
            .method(Method::DELETE)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[ntex_rt::test]
    async fn test_data() {
        let mut srv = init_service(
            App::new()
                .data(1.0f64)
                .data(1usize)
                .app_data(web::types::Data::new('-'))
                .service(
                    web::resource("/test")
                        .data(10usize)
                        .app_data(web::types::Data::new('*'))
                        .guard(guard::Get())
                        .to(
                            |data1: web::types::Data<usize>,
                             data2: web::types::Data<char>,
                             data3: web::types::Data<f64>| {
                                assert_eq!(**data1, 10);
                                assert_eq!(**data2, '*');
                                assert_eq!(**data3, 1.0);
                                ready(HttpResponse::Ok())
                            },
                        ),
                ),
        )
        .await;

        let req = TestRequest::get().uri("/test").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
