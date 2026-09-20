use std::{fmt, mem, rc::Rc};

use crate::error::Failure;
use crate::http::Method;
use crate::service::{Ctx, Service, ServiceFactory};

use super::error::{WebError, WebResponseError};
use super::guard::{self, AllGuard, Guard};
use super::handler::{Handler, HandlerFn, HandlerSt, HandlerStWrapper, HandlerWrapper};
use super::{FromRequest, HttpResponse, State, WebRequest, WebResponse};

/// Connects a handler to the requests it should handle.
///
/// A route belongs to a [`Resource`](super::Resource). It does not define a
/// path itself; instead, its method and custom guards decide which requests
/// within that resource should reach its handler.
///
/// Routes are checked in registration order. A route matches when the request
/// method matches one of its configured methods and every custom guard accepts
/// the request. Without a method or custom guard, it matches any request that
/// reaches the resource.
///
/// If a route does not match, the resource tries the next one. When no route
/// matches, the resource fallback runs and returns `405 Method Not Allowed` by
/// default.
///
/// Use helpers such as [`web::get()`] and [`web::post()`] to create
/// method-specific routes, then attach a handler with [`Route::to()`] or
/// [`Route::to_with_state()`]. Handler arguments are populated through
/// [`FromRequest`], and the returned value is converted into a response through
/// [`Responder`](super::Responder).
///
/// A route without an explicitly configured handler returns `404 Not Found`
/// when called.
///
/// ```rust
/// use ntex::web::{self, App, HttpResponse};
///
/// App::default().service(
///     web::resource("/users/{id}")
///         .route(web::get().to(async || "user"))
///         .route(web::delete().to(async || HttpResponse::NoContent())),
/// );
/// ```
///
/// [`web::get()`]: super::get
/// [`web::post()`]: super::post
pub struct Route<St: State, In = ()> {
    handler: Rc<dyn HandlerFn<St, In>>,
    methods: Vec<Method>,
    guards: Rc<AllGuard>,
}

impl<St: State, In: 'static> Route<St, In> {
    /// Create new route which matches any request.
    pub fn new() -> Route<St, In> {
        Route {
            handler: HandlerWrapper::<St, In, _, ()>::create(async || HttpResponse::NotFound()),
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

    pub(super) fn service(&self) -> RouteService<St, In> {
        RouteService {
            handler: self.handler.clone(),
            guards: self.guards.clone(),
            methods: self.methods.clone(),
        }
    }
}

impl<St: State, In: 'static> Default for Route<St, In> {
    fn default() -> Self {
        Self::new()
    }
}

impl<St: State, In: 'static> ServiceFactory<St, WebRequest<In>> for Route<St, In> {
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    type Service = RouteService<St, In>;
    type InitError = Failure;

    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(self.service())
    }
}

impl<St: State, In: 'static> Route<St, In> {
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
    #[must_use]
    pub fn method(mut self, method: Method) -> Self {
        self.methods.push(method);
        self
    }

    /// Add a match guard to this route.
    ///
    /// All guards registered on the route must accept the request. Guards are
    /// evaluated after the containing resource has matched. If a guard rejects
    /// the request, the resource tries its next route; if no route matches, the
    /// resource's default service is used.
    ///
    /// Method restrictions added by [`Route::method()`] are evaluated together
    /// with these guards.
    ///
    /// ```rust
    /// use ntex::web::{self, guard, App};
    ///
    /// App::default().service(
    ///     web::resource("/items")
    ///         .route(
    ///             web::get()
    ///                 .guard(guard::Header("accept", "application/json"))
    ///                 .to(async || "JSON items")
    ///         )
    ///         .route(web::get().to(async || "Default items"))
    /// );
    /// ```
    #[must_use]
    pub fn guard<F: Guard + 'static>(mut self, f: F) -> Self {
        Rc::get_mut(&mut self.guards).unwrap().add(f);
        self
    }

    /// Set the handler for this route.
    ///
    /// Handler arguments are populated through [`FromRequest`]. Each argument
    /// must implement [`FromRequest`] trait and are evaluated
    /// before the handler is called. If extraction fails, the error is
    /// converted into a response.
    ///
    /// The handler's return value must implement [`Responder`](super::Responder).
    /// Use [`Route::to_with_state()`] when the handler also needs a borrowed
    /// application state and the current request state.
    ///
    /// ```rust
    /// use std::collections::HashMap;
    /// use ntex::web;
    ///
    /// #[derive(serde::Deserialize)]
    /// struct UserPath {
    ///     user_id: u32,
    /// }
    ///
    /// async fn show_user(
    ///     path: web::types::Path<UserPath>,
    ///     query: web::types::Query<HashMap<String, String>>,
    /// ) -> String {
    ///     let format = query.get("format").map(String::as_str).unwrap_or("text");
    ///     format!("User {} as {format}", path.user_id)
    /// }
    ///
    /// web::App::default().service(
    ///     web::resource("/users/{user_id}")
    ///         .route(web::get().to(show_user))
    /// );
    /// ```
    #[must_use]
    pub fn to<H, Args>(mut self, handler: H) -> Self
    where
        H: Handler<St, Args> + 'static,
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St, St::Error>,
    {
        self.handler = HandlerWrapper::create(handler);
        self
    }

    /// Set a state-aware handler for this route.
    ///
    /// The handler receives a shared reference to the application state,
    /// followed by the current request state and any request extractors.
    ///
    /// ```rust
    /// use ntex::web;
    ///
    /// struct AppState {
    ///     greeting: &'static str,
    /// }
    ///
    /// impl web::State for AppState {
    ///     type Error = web::DefaultError;
    /// }
    ///
    /// async fn index(
    ///     state: &AppState,
    ///     request_state: (),
    ///     name: web::types::Path<String>,
    /// ) -> String {
    ///     let _ = request_state;
    ///     format!("{}, {}!", state.greeting, name.into_inner())
    /// }
    ///
    /// web::App::<AppState>::new().service(
    ///     web::resource("/{name}").route(web::get().to_with_state(index))
    /// );
    /// ```
    #[must_use]
    pub fn to_with_state<H, Args>(mut self, handler: H) -> Self
    where
        H: HandlerSt<St, In, Args> + 'static,
        Args: FromRequest<St> + 'static,
        Args::Error: WebResponseError<St, St::Error>,
    {
        self.handler = HandlerStWrapper::create(handler);
        self
    }
}

impl<St: State, In> fmt::Debug for Route<St, In> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route")
            .field("handler", &self.handler)
            .field("methods", &self.methods)
            .field("guards", &self.guards)
            .finish()
    }
}

pub struct RouteService<St: State, In> {
    handler: Rc<dyn HandlerFn<St, In>>,
    methods: Vec<Method>,
    guards: Rc<AllGuard>,
}

impl<St: State, In> RouteService<St, In> {
    pub fn check(&self, req: &mut WebRequest<In>) -> bool {
        if !self.methods.is_empty() && !self.methods.contains(&req.head().method) {
            return false;
        }

        self.guards.check(req.head())
    }
}

impl<St: State, In> fmt::Debug for RouteService<St, In> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteService")
            .field("handler", &self.handler)
            .field("methods", &self.methods)
            .field("guards", &self.guards)
            .finish()
    }
}

impl<St: State, In> Service<St, WebRequest<In>> for RouteService<St, In> {
    type Res = WebResponse;
    type Error = WebError<St, St::Error>;

    async fn call(
        &self,
        req: WebRequest<In>,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        Ok(self.handler.call(ctx.st(), req).await)
    }
}

/// Convert object to a vec of routes
pub trait IntoRoutes<St: State, In> {
    fn routes(self) -> Vec<Route<St, In>>;
}

impl<St: State, In> IntoRoutes<St, In> for Route<St, In> {
    fn routes(self) -> Vec<Route<St, In>> {
        vec![self]
    }
}

impl<St: State, In> IntoRoutes<St, In> for Vec<Route<St, In>> {
    fn routes(self) -> Vec<Route<St, In>> {
        self
    }
}

macro_rules! tuple_routes(
    {$(#[$meta:meta])* $(($n:tt, $T:ident)),+} => {
        $(#[$meta])*
        #[allow(unused_parens)]
        impl<St: State, U, $($T,)+> IntoRoutes<St, U> for ($($T,)+)
        where
            $($T: Into<Route<St, U>> + 'static,)+ {
            fn routes(self) -> Vec<Route<St, U>> {
                vec![$(self.$n.into(),)+]
            }
        }
    }
);

impl<St: State, In, T, const N: usize> IntoRoutes<St, In> for [T; N]
where
    T: Into<Route<St, In>>,
{
    fn routes(self) -> Vec<Route<St, In>> {
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

        let route: web::Route<(), ()> = web::get();
        let repr = format!("{route:?}");
        assert!(repr.contains("Route"), "{}", repr);
        assert!(
            repr.contains(
                "handler: HandlerNoState(\"ntex::web::route::Route<()>::new::{{closure}}\")"
            ),
            "{}",
            repr
        );
        assert!(repr.contains("methods: [GET]"), "{}", repr);
        assert!(repr.contains("guards: AllGuard()"), "{}", repr);

        assert!(route.create(&()).await.is_ok());

        let route_service = route.service();
        let repr = format!("{route_service:?}");
        assert!(repr.contains("RouteService"));
        assert!(repr.contains(
            "handler: HandlerNoState(\"ntex::web::route::Route<()>::new::{{closure}}\")"
        ));
        assert!(repr.contains("methods: [GET]"));
        assert!(repr.contains("guards: AllGuard()"));
    }
}
