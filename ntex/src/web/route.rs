use std::{error::Error, fmt, mem, rc::Rc};

use crate::http::Method;
use crate::service::{Ctx, Service, ServiceFactory};

use super::guard::{self, AllGuard, Guard};
use super::handler::{Handler, HandlerFn, HandlerWrapper};
use super::{AppState, FromRequest, HttpResponse, WebRequest, WebResponse, WebResponseError};

/// Resource route definition
///
/// Route uses builder-like pattern for configuration.
/// If handler is not explicitly set, default *404 Not Found* handler is used.
pub struct Route<St: AppState> {
    handler: Rc<dyn HandlerFn<St>>,
    methods: Vec<Method>,
    guards: Rc<AllGuard>,
}

impl<St: AppState> Route<St> {
    /// Create new route which matches any request.
    pub fn new() -> Route<St> {
        Route {
            handler: Rc::new(HandlerWrapper::<St, _, ()>::new(async || {
                HttpResponse::NotFound()
            })),
            methods: Vec::new(),
            guards: Rc::default(),
        }
    }

    pub(super) fn take_guards(&mut self) -> Vec<Box<dyn Guard>> {
        for m in &self.methods {
            Rc::get_mut(&mut self.guards)
                .unwrap()
                .add(guard::Method(m.clone()));
        }

        mem::take(&mut Rc::get_mut(&mut self.guards).unwrap().0)
    }

    pub(super) fn service(&self) -> RouteService<St> {
        RouteService {
            handler: self.handler.clone(),
            guards: self.guards.clone(),
            methods: self.methods.clone(),
        }
    }
}

impl<St: AppState> Default for Route<St> {
    fn default() -> Self {
        Self::new()
    }
}

impl<St: AppState> ServiceFactory<St, WebRequest> for Route<St> {
    type Res = WebResponse;
    type Error = St::Error;

    type Service = RouteService<St>;
    type InitError = Box<dyn Error>;

    async fn create(&self, _: &St) -> Result<RouteService<St>, Self::InitError> {
        Ok(self.service())
    }
}

impl<St: AppState> Route<St> {
    #[must_use]
    /// Add method guard to the route.
    ///
    /// ```rust
    /// # use ntex::web::{self, *};
    /// # fn main() {
    /// App::default().service(web::resource("/path").route(
    ///     web::route()
    ///         .method(ntex::http::Method::CONNECT)
    ///         .guard(guard::Header("content-type", "text/plain"))
    ///         .to(async |req: HttpRequest| { HttpResponse::Ok() }))
    /// );
    /// # }
    /// ```
    pub fn method(mut self, method: Method) -> Self {
        self.methods.push(method);
        self
    }

    #[must_use]
    /// Add guard to the route.
    ///
    /// ```rust
    /// # use ntex::web::{self, *};
    /// # fn main() {
    /// App::default().service(web::resource("/path").route(
    ///     web::route()
    ///         .guard(guard::Get())
    ///         .guard(guard::Header("content-type", "text/plain"))
    ///         .to(async |req: HttpRequest| { HttpResponse::Ok() }))
    /// );
    /// # }
    /// ```
    pub fn guard<F: Guard + 'static>(mut self, f: F) -> Self {
        Rc::get_mut(&mut self.guards).unwrap().add(f);
        self
    }

    #[must_use]
    /// Set handler function, use request extractors for parameters.
    ///
    /// ```rust
    /// use ntex::web;
    ///
    /// #[derive(serde::Deserialize)]
    /// struct Info {
    ///     username: String,
    /// }
    ///
    /// /// extract path info using serde
    /// async fn index(info: web::types::Path<Info>) -> String {
    ///     format!("Welcome {}!", info.username)
    /// }
    ///
    /// fn main() {
    ///     let app = web::App::default().service(
    ///         web::resource("/{username}/index.html") // <- define path parameters
    ///             .route(web::get().to(index))        // <- register handler
    ///     );
    /// }
    /// ```
    ///
    /// It is possible to use multiple extractors for one handler function.
    ///
    /// ```rust
    /// # use std::collections::HashMap;
    /// use ntex::web;
    ///
    /// #[derive(serde::Deserialize)]
    /// struct Info {
    ///     username: String,
    /// }
    ///
    /// /// extract path info using serde
    /// async fn index(path: web::types::Path<Info>, query: web::types::Query<HashMap<String, String>>, body: web::types::Json<Info>) -> String {
    ///     format!("Welcome {}!", path.username)
    /// }
    ///
    /// fn main() {
    ///     let app = web::App::default().service(
    ///         web::resource("/{username}/index.html") // <- define path parameters
    ///             .route(web::get().to(index))
    ///     );
    /// }
    /// ```
    pub fn to<H, Args>(mut self, handler: H) -> Self
    where
        H: Handler<St, Args> + 'static,
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St::Error>,
    {
        self.handler = Rc::new(HandlerWrapper::new(handler));
        self
    }
}

impl<St: AppState> fmt::Debug for Route<St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("handler", &self.handler)
            .field("methods", &self.methods)
            .field("guards", &self.guards)
            .finish()
    }
}

pub struct RouteService<St: AppState> {
    handler: Rc<dyn HandlerFn<St>>,
    methods: Vec<Method>,
    guards: Rc<AllGuard>,
}

impl<St: AppState> RouteService<St> {
    pub fn check(&self, req: &mut WebRequest) -> bool {
        if !self.methods.is_empty() && !self.methods.contains(&req.head().method) {
            return false;
        }

        self.guards.check(req.head())
    }
}

impl<St: AppState> fmt::Debug for RouteService<St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteService")
            .field("handler", &self.handler)
            .field("methods", &self.methods)
            .field("guards", &self.guards)
            .finish()
    }
}

impl<St: AppState> Service<St, WebRequest> for RouteService<St> {
    type Res = WebResponse;
    type Error = St::Error;

    async fn call(
        &self,
        req: WebRequest,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        self.handler.call(ctx.st(), req).await
    }
}

/// Convert object to a vec of routes
pub trait IntoRoutes<St: AppState> {
    fn routes(self) -> Vec<Route<St>>;
}

impl<St: AppState> IntoRoutes<St> for Route<St> {
    fn routes(self) -> Vec<Route<St>> {
        vec![self]
    }
}

impl<St: AppState> IntoRoutes<St> for Vec<Route<St>> {
    fn routes(self) -> Vec<Route<St>> {
        self
    }
}

macro_rules! tuple_routes(
    {$(#[$meta:meta])* $(($n:tt, $T:ident)),+} => {
        $(#[$meta])*
        #[allow(unused_parens)]
        impl<St: AppState, $($T,)+> IntoRoutes<St> for ($($T,)+)
        where
            $($T: Into<Route<St>> + 'static,)+ {
            fn routes(self) -> Vec<Route<St>> {
                vec![$(self.$n.into(),)+]
            }
        }
    }
);

impl<St: AppState, T, const N: usize> IntoRoutes<St> for [T; N]
where
    T: Into<Route<St>>,
{
    fn routes(self) -> Vec<Route<St>> {
        let mut routes = Vec::with_capacity(N);
        for route in self {
            routes.push(route.into());
        }
        routes
    }
}

#[allow(clippy::wildcard_imports)]
#[rustfmt::skip]
mod m {
    use variadics_please::all_tuples_enumerated;

    use super::*;

    all_tuples_enumerated!(#[doc(fake_variadic)] tuple_routes, 1, 12, T);
}

#[cfg(test)]
mod tests {
    use crate::http::{Method, StatusCode, header};
    use crate::time::{Millis, sleep};
    use crate::web::test::{TestRequest, call_service, init_service, read_body};
    use crate::web::{self, App, HttpResponse, error, guard};
    use crate::{ServiceFactory, util::Bytes};

    #[derive(serde::Serialize, PartialEq, Debug)]
    struct MyObject {
        name: String,
    }

    #[crate::rt_test]
    async fn test_route() {
        let srv = init_service(
            App::new()
                .service(web::resource("/test").route(vec![
                        web::get().to(async || { HttpResponse::Ok() }),
                        web::put().to(async || {
                            Err::<HttpResponse, _>(
                                error::ErrorBadRequest::<_>("err"),
                            )
                        }),
                        web::post().to(async || {
                            sleep(Millis(100)).await;
                            HttpResponse::Created()
                        }),
                        web::patch()
                            .guard(guard::fn_guard(|req|
                                req.headers().contains_key("content-type")
                            ))
                            .to(async || { HttpResponse::Conflict() }),
                        web::delete().to(async || {
                            sleep(Millis(100)).await;
                            Err::<HttpResponse, _>(error::ErrorBadRequest("err"))
                        }),
                    ]))
                .service(web::resource("/json").route(web::get().to(async || {
                    sleep(Millis(25)).await;
                    web::types::Json(MyObject {
                        name: "test".to_string(),
                    })
                }))),
        )
        .await;

        let req = TestRequest::with_uri("/test")
            .method(Method::GET)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test")
            .method(Method::POST)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = TestRequest::with_uri("/test")
            .method(Method::PUT)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let req = TestRequest::with_uri("/test")
            .method(Method::PATCH)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let req = TestRequest::with_uri("/test")
            .method(Method::PATCH)
            .header(header::CONTENT_TYPE, "text/plain")
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        let req = TestRequest::with_uri("/test")
            .method(Method::DELETE)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let req = TestRequest::with_uri("/test")
            .method(Method::HEAD)
            .to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let req = TestRequest::with_uri("/json").to_request();
        let resp = call_service(&srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = read_body(resp).await;
        assert_eq!(body, Bytes::from_static(b"{\"name\":\"test\"}"));

        let route: web::Route<()> = web::get();
        let repr = format!("{route:?}");
        assert!(repr.contains("Route"), "{}", repr);
        assert!(
            repr.contains("handler: Handler(\"ntex::web::route::Route<()>::new::{{closure}}\")"),
            "{}",
            repr
        );
        assert!(repr.contains("methods: [GET]"), "{}", repr);
        assert!(repr.contains("guards: AllGuard()"), "{}", repr);

        assert!(route.create(&()).await.is_ok());

        let route_service = route.service();
        let repr = format!("{route_service:?}");
        assert!(repr.contains("RouteService"));
        assert!(
            repr.contains("handler: Handler(\"ntex::web::route::Route<()>::new::{{closure}}\")")
        );
        assert!(repr.contains("methods: [GET]"));
        assert!(repr.contains("guards: AllGuard()"));
    }
}
