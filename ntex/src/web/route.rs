use std::mem;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::future::{ok, ready, LocalBoxFuture, Ready};

use crate::http::Method;
use crate::{Service, ServiceFactory};

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
            handler: Box::new(HandlerWrapper::new(|| ready(HttpResponse::NotFound()))),
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

        mem::replace(Rc::get_mut(&mut self.guards).unwrap(), Vec::new())
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
    type Future = Ready<Result<RouteService<Err>, ()>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.service())
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
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

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
    /// use serde_derive::Deserialize;
    ///
    /// #[derive(Deserialize)]
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
    /// # use serde_derive::Deserialize;
    /// use ntex::web;
    ///
    /// #[derive(Deserialize)]
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
        <F::Result as Responder<Err>>::Error: Into<Err::Container>,
    {
        self.handler = Box::new(HandlerWrapper::new(handler));
        self
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use serde_derive::Serialize;

    use crate::http::{Method, StatusCode};
    use crate::rt::time::delay_for;
    use crate::web::test::{call_service, init_service, read_body, TestRequest};
    use crate::web::{self, error, App, DefaultError, HttpResponse};

    #[derive(Serialize, PartialEq, Debug)]
    struct MyObject {
        name: String,
    }

    #[ntex_rt::test]
    async fn test_route() {
        let mut srv = init_service(
            App::new()
                .service(
                    web::resource("/test")
                        .route(web::get().to(|| async { HttpResponse::Ok() }))
                        .route(web::put().to(|| async {
                            Err::<HttpResponse, _>(error::ErrorBadRequest::<
                                _,
                                DefaultError,
                            >("err"))
                        }))
                        .route(web::post().to(|| async {
                            delay_for(Duration::from_millis(100)).await;
                            HttpResponse::Created()
                        }))
                        .route(web::delete().to(|| async {
                            delay_for(Duration::from_millis(100)).await;
                            Err::<HttpResponse, _>(error::ErrorBadRequest("err"))
                        })),
                )
                .service(web::resource("/json").route(web::get().to(|| async {
                    delay_for(Duration::from_millis(25)).await;
                    web::types::Json(MyObject {
                        name: "test".to_string(),
                    })
                }))),
        )
        .await;

        let req = TestRequest::with_uri("/test")
            .method(Method::GET)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let req = TestRequest::with_uri("/test")
            .method(Method::POST)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let req = TestRequest::with_uri("/test")
            .method(Method::PUT)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let req = TestRequest::with_uri("/test")
            .method(Method::DELETE)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let req = TestRequest::with_uri("/test")
            .method(Method::HEAD)
            .to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let req = TestRequest::with_uri("/json").to_request();
        let resp = call_service(&mut srv, req).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = read_body(resp).await;
        assert_eq!(body, Bytes::from_static(b"{\"name\":\"test\"}"));
    }
}
