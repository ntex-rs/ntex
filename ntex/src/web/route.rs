use std::{future::Future, mem, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::{http::Method, util::Ready, Service, ServiceFactory};

use super::error::ErrorRenderer;
use super::error_default::DefaultError;
use super::extract::FromRequest;
use super::guard::{self, Guard};
use super::handler::{Handler, HandlerFn, HandlerWrapper};
use super::request::WebRequest;
use super::responder::Responder;
use super::response::WebResponse;
use super::HttpResponse;

/// Resource route definition
///
/// Route uses builder-like pattern for configuration.
/// If handler is not explicitly set, default *404 Not Found* handler is used.
pub struct Route<Err: ErrorRenderer = DefaultError> {
    handler: Box<dyn HandlerFn<Err>>,
    methods: Vec<Method>,
    guards: Rc<Vec<Box<dyn Guard>>>,
}

impl<Err: ErrorRenderer> Route<Err> {
    /// Create new route which matches any request.
    pub fn new() -> Route<Err> {
        Route {
            handler: Box::new(HandlerWrapper::new(|| async {
                HttpResponse::NotFound()
            })),
            methods: Vec::new(),
            guards: Rc::new(Vec::new()),
        }
    }

    pub(super) fn take_guards(&mut self) -> Vec<Box<dyn Guard>> {
        for m in &self.methods {
            Rc::get_mut(&mut self.guards)
                .unwrap()
                .push(Box::new(guard::Method(m.clone())));
        }

        mem::take(Rc::get_mut(&mut self.guards).unwrap())
    }

    pub(super) fn service(&self) -> RouteService<Err> {
        RouteService {
            handler: self.handler.clone_handler(),
            guards: self.guards.clone(),
            methods: self.methods.clone(),
        }
    }
}

impl<Err: ErrorRenderer> ServiceFactory for Route<Err> {
    type Config = ();
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type InitError = ();
    type Service = RouteService<Err>;
    type Future = Ready<RouteService<Err>, ()>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ok(self.service()).into()
    }
}

pub struct RouteService<Err: ErrorRenderer> {
    handler: Box<dyn HandlerFn<Err>>,
    methods: Vec<Method>,
    guards: Rc<Vec<Box<dyn Guard>>>,
}

impl<Err: ErrorRenderer> RouteService<Err> {
    pub fn check(&self, req: &mut WebRequest<Err>) -> bool {
        if !self.methods.is_empty() && !self.methods.contains(&req.head().method) {
            return false;
        }

        for f in self.guards.iter() {
            if !f.check(req.head()) {
                return false;
            }
        }
        true
    }
}

impl<Err: ErrorRenderer> Service for RouteService<Err> {
    type Request = WebRequest<Err>;
    type Response = WebResponse;
    type Error = Err::Container;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: WebRequest<Err>) -> Self::Future {
        self.handler.call(req)
    }
}

impl<Err: ErrorRenderer> Route<Err> {
    /// Add method guard to the route.
    ///
    /// ```rust
    /// # use ntex::web::{self, *};
    /// # fn main() {
    /// App::new().service(web::resource("/path").route(
    ///     web::route()
    ///         .method(http::Method::CONNECT)
    ///         .guard(guard::Header("content-type", "text/plain"))
    ///         .to(|req: HttpRequest| async { HttpResponse::Ok() }))
    /// );
    /// # }
    /// ```
    pub fn method(mut self, method: Method) -> Self {
        self.methods.push(method);
        self
    }

    /// Add guard to the route.
    ///
    /// ```rust
    /// # use ntex::web::{self, *};
    /// # fn main() {
    /// App::new().service(web::resource("/path").route(
    ///     web::route()
    ///         .guard(guard::Get())
    ///         .guard(guard::Header("content-type", "text/plain"))
    ///         .to(|req: HttpRequest| async { HttpResponse::Ok() }))
    /// );
    /// # }
    /// ```
    pub fn guard<F: Guard + 'static>(mut self, f: F) -> Self {
        Rc::get_mut(&mut self.guards).unwrap().push(Box::new(f));
        self
    }

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
    ///     let app = web::App::new().service(
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
    ///     let app = web::App::new().service(
    ///         web::resource("/{username}/index.html") // <- define path parameters
    ///             .route(web::get().to(index))
    ///     );
    /// }
    /// ```
    pub fn to<F, Args>(mut self, handler: F) -> Self
    where
        F: Handler<Args, Err>,
        Args: FromRequest<Err> + 'static,
        Args::Error: Into<Err::Container>,
        <F::Output as Responder<Err>>::Error: Into<Err::Container>,
    {
        self.handler = Box::new(HandlerWrapper::new(handler));
        self
    }
}

/// Convert object to a vec of routes
pub trait IntoRoutes<Err: ErrorRenderer> {
    fn routes(self) -> Vec<Route<Err>>;
}

impl<Err: ErrorRenderer> IntoRoutes<Err> for Route<Err> {
    fn routes(self) -> Vec<Route<Err>> {
        vec![self]
    }
}

impl<Err: ErrorRenderer> IntoRoutes<Err> for Vec<Route<Err>> {
    fn routes(self) -> Vec<Route<Err>> {
        self
    }
}

macro_rules! tuple_routes({$(($n:tt, $T:ty)),+} => {
    /// IntoRoutes implementation for a tuple
    #[allow(unused_parens)]
    impl<Err: ErrorRenderer> IntoRoutes<Err> for ($($T,)+) {
        fn routes(self) -> Vec<Route<Err>> {
            vec![$(self.$n,)+]
        }
    }
});

macro_rules! array_routes({$num:tt, $($T:ident),+} => {
    /// IntoRoutes implementation for an array
    #[allow(unused_parens)]
    impl<Err: ErrorRenderer> IntoRoutes<Err> for [Route<Err>; $num]
    where
        Err: ErrorRenderer,
    {
        fn routes(self) -> Vec<Route<Err>> {
            let [$($T,)+] = self;
            vec![$($T,)+]
        }
    }
});

#[rustfmt::skip]
mod m {
    use super::*;

tuple_routes!((0,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>),(5,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>),(5,Route<Err>),(6,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>),(5,Route<Err>),(6,Route<Err>),(7,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>),(5,Route<Err>),(6,Route<Err>),(7,Route<Err>),(8,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>),(5,Route<Err>),(6,Route<Err>),(7,Route<Err>),(8,Route<Err>),(9,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>),(5,Route<Err>),(6,Route<Err>),(7,Route<Err>),(8,Route<Err>),(9,Route<Err>),(10,Route<Err>));
tuple_routes!((0,Route<Err>),(1,Route<Err>),(2,Route<Err>),(3,Route<Err>),(4,Route<Err>),(5,Route<Err>),(6,Route<Err>),(7,Route<Err>),(8,Route<Err>),(9,Route<Err>),(10,Route<Err>),(11,Route<Err>));
}

array_routes!(1, a);
array_routes!(2, a, b);
array_routes!(3, a, b, c);
array_routes!(4, a, b, c, d);
array_routes!(5, a, b, c, d, e);
array_routes!(6, a, b, c, d, e, f);
array_routes!(7, a, b, c, d, e, f, g);
array_routes!(8, a, b, c, d, e, f, g, h);
array_routes!(9, a, b, c, d, e, f, g, h, i);
array_routes!(10, a, b, c, d, e, f, g, h, i, j);
array_routes!(11, a, b, c, d, e, f, g, h, i, j, k);
array_routes!(12, a, b, c, d, e, f, g, h, i, j, k, l);

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use crate::http::{Method, StatusCode};
    use crate::rt::time::sleep;
    use crate::web::test::{call_service, init_service, read_body, TestRequest};
    use crate::web::{self, error, App, DefaultError, HttpResponse};

    #[derive(serde::Serialize, PartialEq, Debug)]
    struct MyObject {
        name: String,
    }

    #[crate::rt_test]
    async fn test_route() {
        let srv = init_service(
            App::new()
                .service(web::resource("/test").route(vec![
                    web::get().to(|| async { HttpResponse::Ok() }),
                    web::put().to(|| async {
                        Err::<HttpResponse, _>(
                            error::ErrorBadRequest::<_, DefaultError>("err"),
                        )
                    }),
                    web::post().to(|| async {
                        sleep(Duration::from_millis(100)).await;
                        HttpResponse::Created()
                    }),
                    web::delete().to(|| async {
                        sleep(Duration::from_millis(100)).await;
                        Err::<HttpResponse, _>(error::ErrorBadRequest("err"))
                    }),
                ]))
                .service(web::resource("/json").route(web::get().to(|| async {
                    sleep(Duration::from_millis(25)).await;
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
    }
}
