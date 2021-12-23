use std::{
    cell::RefCell, fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use crate::http::Response;
use crate::router::{IntoPattern, ResourceDef};
use crate::service::boxed::{self, BoxService, BoxServiceFactory};
use crate::service::{pipeline_factory, PipelineFactory};
use crate::service::{Identity, IntoServiceFactory, Service, ServiceFactory, Transform};
use crate::util::{Either, Extensions, Ready};

use super::dev::{insert_slesh, WebServiceConfig, WebServiceFactory};
use super::error::ErrorRenderer;
use super::extract::FromRequest;
use super::guard::Guard;
use super::handler::Handler;
use super::request::WebRequest;
use super::responder::Responder;
use super::response::WebResponse;
use super::route::{IntoRoutes, Route, RouteService};
use super::{app::Filter, app::Stack, types::Data};

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
pub struct Resource<Err: ErrorRenderer, M = Identity, T = Filter<Err>> {
    middleware: M,
    filter: PipelineFactory<T, WebRequest<Err>>,
    rdef: Vec<String>,
    name: Option<String>,
    routes: Vec<Route<Err>>,
    data: Option<Extensions>,
    guards: Vec<Box<dyn Guard>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
}

impl<Err: ErrorRenderer> Resource<Err> {
    pub fn new<T: IntoPattern>(path: T) -> Resource<Err> {
        Resource {
            routes: Vec::new(),
            rdef: path.patterns(),
            name: None,
            middleware: Identity,
            filter: pipeline_factory(Filter::new()),
            guards: Vec::new(),
            data: None,
            default: Rc::new(RefCell::new(None)),
        }
    }
}

impl<Err, M, T> Resource<Err, M, T>
where
    T: ServiceFactory<
        WebRequest<Err>,
        Config = (),
        Response = WebRequest<Err>,
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
    pub fn data<D: 'static>(self, data: D) -> Self {
        self.app_data(Data::new(data))
    }

    /// Set or override application data.
    ///
    /// This method overrides data stored with [`App::app_data()`](#method.app_data)
    pub fn app_data<D: 'static>(mut self, data: D) -> Self {
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
            Config = (),
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
    >
    where
        U: ServiceFactory<
            WebRequest<Err>,
            Config = (),
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        >,
        F: IntoServiceFactory<U, WebRequest<Err>>,
    {
        Resource {
            filter: self.filter.and_then(filter.into_factory()),
            middleware: self.middleware,
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
            data: self.data,
        }
    }

    /// Register a resource middleware.
    ///
    /// This is similar to `App's` middlewares, but middleware get invoked on resource level.
    /// Resource level middlewares are not allowed to change response
    /// type (i.e modify response's body).
    ///
    /// **Note**: middlewares get called in opposite order of middlewares registration.
    pub fn wrap<U>(self, mw: U) -> Resource<Err, Stack<M, U>, T> {
        Resource {
            middleware: Stack::new(self.middleware, mw),
            filter: self.filter,
            rdef: self.rdef,
            name: self.name,
            guards: self.guards,
            routes: self.routes,
            default: self.default,
            data: self.data,
        }
    }

    /// Default service to be used if no matching route could be found.
    /// By default *405* response get returned. Resource does not use
    /// default handler from `App` or `Scope`.
    pub fn default_service<F, S>(mut self, f: F) -> Self
    where
        F: IntoServiceFactory<S, WebRequest<Err>>,
        S: ServiceFactory<
                WebRequest<Err>,
                Config = (),
                Response = WebResponse,
                Error = Err::Container,
            > + 'static,
        S::InitError: fmt::Debug,
    {
        // create and configure default resource
        self.default = Rc::new(RefCell::new(Some(Rc::new(boxed::factory(
            f.into_factory().map_init_err(|e| {
                log::error!("Cannot construct default service: {:?}", e)
            }),
        )))));

        self
    }
}

impl<Err, M, T> WebServiceFactory<Err> for Resource<Err, M, T>
where
    T: ServiceFactory<
            WebRequest<Err>,
            Config = (),
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    M: Transform<ResourceService<T::Service, Err>> + 'static,
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

        let router_factory = ResourceRouterFactory {
            routes: self.routes,
            data: self.data.map(Rc::new),
            default: self.default,
        };

        config.register_service(
            rdef,
            guards,
            ResourceServiceFactory {
                middleware: Rc::new(self.middleware),
                filter: self.filter,
                routing: router_factory,
            },
            None,
        )
    }
}

impl<Err, M, T>
    IntoServiceFactory<
        ResourceServiceFactory<Err, M, PipelineFactory<T, WebRequest<Err>>>,
        WebRequest<Err>,
    > for Resource<Err, M, T>
where
    T: ServiceFactory<
            WebRequest<Err>,
            Config = (),
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    M: Transform<ResourceService<T::Service, Err>> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    Err: ErrorRenderer,
{
    fn into_factory(
        self,
    ) -> ResourceServiceFactory<Err, M, PipelineFactory<T, WebRequest<Err>>> {
        let router_factory = ResourceRouterFactory {
            routes: self.routes,
            data: self.data.map(Rc::new),
            default: self.default,
        };

        ResourceServiceFactory {
            middleware: Rc::new(self.middleware),
            filter: self.filter,
            routing: router_factory,
        }
    }
}

/// Resource service
pub struct ResourceServiceFactory<Err: ErrorRenderer, M, T> {
    middleware: Rc<M>,
    filter: T,
    routing: ResourceRouterFactory<Err>,
}

impl<Err, M, F> ServiceFactory<WebRequest<Err>> for ResourceServiceFactory<Err, M, F>
where
    M: Transform<ResourceService<F::Service, Err>> + 'static,
    M::Service: Service<WebRequest<Err>, Response = WebResponse, Error = Err::Container>,
    F: ServiceFactory<
            WebRequest<Err>,
            Config = (),
            Response = WebRequest<Err>,
            Error = Err::Container,
            InitError = (),
        > + 'static,
    Err: ErrorRenderer,
{
    type Config = ();
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
            Ok(middleware.new_transform(ResourceService {
                filter: filter_fut.await?,
                routing: Rc::new(routing_fut.await?),
            }))
        })
    }
}

pub struct ResourceService<F, Err: ErrorRenderer> {
    filter: F,
    routing: Rc<ResourceRouter<Err>>,
}

impl<F, Err> Service<WebRequest<Err>> for ResourceService<F, Err>
where
    F: Service<WebRequest<Err>, Response = WebRequest<Err>, Error = Err::Container>,
    Err: ErrorRenderer,
{
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = ResourceServiceResponse<F, Err>;

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
        ResourceServiceResponse {
            filter: self.filter.call(req),
            routing: self.routing.clone(),
            endpoint: None,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct ResourceServiceResponse<F: Service<WebRequest<Err>>, Err: ErrorRenderer> {
        #[pin]
        filter: F::Future,
        routing: Rc<ResourceRouter<Err>>,
        endpoint: Option<<ResourceRouter<Err> as Service<WebRequest<Err>>>::Future>,
    }
}

impl<F, Err> Future for ResourceServiceResponse<F, Err>
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

struct ResourceRouterFactory<Err: ErrorRenderer> {
    routes: Vec<Route<Err>>,
    data: Option<Rc<Extensions>>,
    default: Rc<RefCell<Option<Rc<HttpNewService<Err>>>>>,
}

impl<Err: ErrorRenderer> ServiceFactory<WebRequest<Err>> for ResourceRouterFactory<Err> {
    type Config = ();
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = ResourceRouter<Err>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let data = self.data.clone();
        let routes = self.routes.iter().map(|route| route.service()).collect();
        let default_fut = self.default.borrow().as_ref().map(|f| f.new_service(()));

        Box::pin(async move {
            let default = if let Some(fut) = default_fut {
                Some(fut.await?)
            } else {
                None
            };

            Ok(ResourceRouter {
                routes,
                data,
                default,
            })
        })
    }
}

struct ResourceRouter<Err: ErrorRenderer> {
    routes: Vec<RouteService<Err>>,
    data: Option<Rc<Extensions>>,
    default: Option<HttpService<Err>>,
}

impl<Err: ErrorRenderer> Service<WebRequest<Err>> for ResourceRouter<Err> {
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = Either<
        Ready<WebResponse, Err::Container>,
        Pin<Box<dyn Future<Output = Result<WebResponse, Err::Container>>>>,
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
            Either::Left(Ready::Ok(WebResponse::new(
                Response::MethodNotAllowed().finish(),
                req.into_parts().0,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::http::header::{self, HeaderValue};
    use crate::http::{Method, StatusCode};
    use crate::time::{sleep, Millis};
    use crate::web::middleware::DefaultHeaders;
    use crate::web::test::{call_service, init_service, TestRequest};
    use crate::web::{
        self, guard, request::WebRequest, App, DefaultError, HttpResponse,
    };
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
        let srv =
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
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            HeaderValue::from_static("0001")
        );
    }

    #[crate::rt_test]
    async fn test_to() {
        let srv =
            init_service(App::new().service(web::resource("/test").to(|| async {
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
    async fn test_data() {
        let srv = init_service(
            App::new()
                .data(1i32)
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
                             data3: web::types::Data<i32>| {
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
