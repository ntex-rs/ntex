use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::future::{ready, FutureExt, LocalBoxFuture};

use crate::http::Method;
use crate::{Service, ServiceFactory};

use super::error::WebError;
use super::error_default::DefaultError;
use super::extract::FromRequest;
use super::guard::{self, Guard};
use super::handler::{Extract, Factory, Handler};
use super::request::HttpRequest;
use super::responder::Responder;
use super::service::{WebRequest, WebResponse};
use super::HttpResponse;

type BoxedRouteService<Err> = Box<
    dyn Service<
        Request = WebRequest,
        Response = WebResponse,
        Error = WebError<Err>,
        Future = LocalBoxFuture<'static, Result<WebResponse, WebError<Err>>>,
    >,
>;

type BoxedRouteNewService<Err> = Box<
    dyn ServiceFactory<
        Config = (),
        Request = WebRequest,
        Response = WebResponse,
        Error = WebError<Err>,
        InitError = (),
        Service = BoxedRouteService<Err>,
        Future = LocalBoxFuture<'static, Result<BoxedRouteService<Err>, ()>>,
    >,
>;

/// Resource route definition
///
/// Route uses builder-like pattern for configuration.
/// If handler is not explicitly set, default *404 Not Found* handler is used.
pub struct Route<Err = DefaultError> {
    service: BoxedRouteNewService<Err>,
    guards: Rc<Vec<Box<dyn Guard>>>,
}

impl<Err: 'static> Route<Err> {
    /// Create new route which matches any request.
    pub fn new() -> Route<Err> {
        Route {
            service: Box::new(RouteNewService::new(Extract::new(Handler::new(|| {
                ready(HttpResponse::NotFound())
            })))),
            guards: Rc::new(Vec::new()),
        }
    }

    pub(crate) fn take_guards(&mut self) -> Vec<Box<dyn Guard>> {
        std::mem::replace(Rc::get_mut(&mut self.guards).unwrap(), Vec::new())
    }
}

impl<Err: 'static> ServiceFactory for Route<Err> {
    type Config = ();
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type InitError = ();
    type Service = RouteService<Err>;
    type Future = CreateRouteService<Err>;

    fn new_service(&self, _: ()) -> Self::Future {
        CreateRouteService {
            fut: self.service.new_service(()),
            guards: self.guards.clone(),
        }
    }
}

type RouteFuture<Err> = LocalBoxFuture<'static, Result<BoxedRouteService<Err>, ()>>;

#[pin_project::pin_project]
pub struct CreateRouteService<Err> {
    #[pin]
    fut: RouteFuture<Err>,
    guards: Rc<Vec<Box<dyn Guard>>>,
}

impl<Err> Future for CreateRouteService<Err> {
    type Output = Result<RouteService<Err>, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match this.fut.poll(cx)? {
            Poll::Ready(service) => Poll::Ready(Ok(RouteService {
                service,
                guards: this.guards.clone(),
            })),
            Poll::Pending => Poll::Pending,
        }
    }
}

pub struct RouteService<Err> {
    service: BoxedRouteService<Err>,
    guards: Rc<Vec<Box<dyn Guard>>>,
}

impl<Err> RouteService<Err> {
    pub fn check(&self, req: &mut WebRequest) -> bool {
        for f in self.guards.iter() {
            if !f.check(req.head()) {
                return false;
            }
        }
        true
    }
}

impl<Err: 'static> Service for RouteService<Err> {
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: WebRequest) -> Self::Future {
        self.service.call(req).boxed_local()
    }
}

impl<Err: 'static> Route<Err> {
    /// Add method guard to the route.
    ///
    /// ```rust
    /// # use ntex::web::{self, *};
    /// # fn main() {
    /// App::new().service(web::resource("/path").route(
    ///     web::get()
    ///         .method(http::Method::CONNECT)
    ///         .guard(guard::Header("content-type", "text/plain"))
    ///         .to(|req: HttpRequest| async { HttpResponse::Ok() }))
    /// );
    /// # }
    /// ```
    pub fn method(mut self, method: Method) -> Self {
        Rc::get_mut(&mut self.guards)
            .unwrap()
            .push(Box::new(guard::Method(method)));
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
    pub fn to<F, T, R, U>(mut self, handler: F) -> Self
    where
        F: Factory<T, R, U, Err>,
        T: FromRequest<Err> + 'static,
        R: Future<Output = U> + 'static,
        U: Responder<Err> + 'static,
    {
        self.service =
            Box::new(RouteNewService::new(Extract::new(Handler::new(handler))));
        self
    }
}

struct RouteNewService<T, Err>
where
    T: ServiceFactory<Request = WebRequest, Error = (WebError<Err>, HttpRequest)>,
{
    service: T,
}

impl<T, Err> RouteNewService<T, Err>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest,
        Response = WebResponse,
        Error = (WebError<Err>, HttpRequest),
    >,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service>::Future: 'static,
{
    pub fn new(service: T) -> Self {
        RouteNewService { service }
    }
}

impl<T, Err> ServiceFactory for RouteNewService<T, Err>
where
    T: ServiceFactory<
        Config = (),
        Request = WebRequest,
        Response = WebResponse,
        Error = (WebError<Err>, HttpRequest),
    >,
    T::Future: 'static,
    T::Service: 'static,
    <T::Service as Service>::Future: 'static,
    Err: 'static,
{
    type Config = ();
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type InitError = ();
    type Service = BoxedRouteService<Err>;
    type Future = LocalBoxFuture<'static, Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        self.service
            .new_service(())
            .map(|result| match result {
                Ok(service) => {
                    let service: BoxedRouteService<Err> =
                        Box::new(RouteServiceWrapper { service });
                    Ok(service)
                }
                Err(_) => Err(()),
            })
            .boxed_local()
    }
}

struct RouteServiceWrapper<T: Service<Error = (WebError<Err>, HttpRequest)>, Err> {
    service: T,
}

impl<T, Err> Service for RouteServiceWrapper<T, Err>
where
    T::Future: 'static,
    T: Service<
        Request = WebRequest,
        Response = WebResponse,
        Error = (WebError<Err>, HttpRequest),
    >,
{
    type Request = WebRequest;
    type Response = WebResponse;
    type Error = WebError<Err>;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(|(e, _)| e)
    }

    fn call(&mut self, req: WebRequest) -> Self::Future {
        self.service
            .call(req)
            .map(|res| match res {
                Ok(res) => Ok(res),
                Err((e, _)) => Err(e),
            })
            .boxed_local()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use actix_rt::time::delay_for;
    use bytes::Bytes;
    use serde_derive::Serialize;

    use crate::http::{Method, StatusCode};
    use crate::web::test::{call_service, init_service, read_body, TestRequest};
    use crate::web::{self, error, App, HttpResponse};

    #[derive(Serialize, PartialEq, Debug)]
    struct MyObject {
        name: String,
    }

    #[actix_rt::test]
    async fn test_route() {
        let mut srv = init_service(
            App::new()
                .service(
                    web::resource("/test")
                        .route(web::get().to(|| async { HttpResponse::Ok() }))
                        .route(web::put().to(|| async {
                            Err::<HttpResponse, _>(error::ErrorBadRequest("err"))
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
