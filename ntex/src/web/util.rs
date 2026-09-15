//! Essentials helper functions and types for application registration.
use std::fmt;

use ntex_router::IntoPattern;

use crate::error::IntoFailure;
use crate::http::error::{BlockingError, ResponseError};
use crate::http::header::ContentEncoding;
use crate::http::{Method, Request, Response};
use crate::server::{NoConfig, ServerAppConfig};
use crate::service::{IntoServiceFactory, ServiceFactory};

use super::extract::FromRequest;
use super::handler::Handler;
use super::resource::Resource;
use super::route::Route;
use super::scope::Scope;
use super::server::HttpServer;
use super::service::WebServiceAdapter;
use super::{HttpResponse, HttpResponseBuilder, State, WebResponseError};

/// Create resource for a specific path.
///
/// Resources may have variable path segments. For example, a
/// resource with the path `/a/{name}/c` would match all incoming
/// requests with paths such as `/a/b/c`, `/a/1/c`, or `/a/etc/c`.
///
/// A variable segment is specified in the form `{identifier}`,
/// where the identifier can be used later in a request handler to
/// access the matched value for that segment. This is done by
/// looking up the identifier in the `Params` object returned by
/// `HttpRequest.match_info()` method.
///
/// By default, each segment matches the regular expression `[^{}/]+`.
///
/// You can also specify a custom regex in the form `{identifier:regex}`:
///
/// For instance, to route `GET`-requests on any route matching
/// `/users/{userid}/{friend}` and store `userid` and `friend` in
/// the exposed `Params` object:
///
/// ```rust,no_run
/// use ntex::web;
///
/// #[ntex::main]
/// async fn main() -> std::io::Result<()> {
///     web::server(async |_| {
///         web::App::new().service(
///             web::resource("/users/{userid}/{friend}")
///                 .route(web::get().to(async || { web::HttpResponse::Ok() }))
///                 .route(web::head().to(async || { web::HttpResponse::MethodNotAllowed() }))
///         )
///    })
///    .bind("127.0.0.1:59090", ntex::SharedCfg::default())?
///    .run()
///    .await
/// }
/// ```
pub fn resource<St: State, In: 'static, T: IntoPattern>(path: T) -> Resource<St, In> {
    Resource::new(path)
}

/// Configure scope for common root path.
///
/// Scopes collect multiple paths under a common path prefix.
/// Scope path can contain variable path segments as resources.
///
/// ```rust,no_run
/// use ntex::web;
///
/// #[ntex::main]
/// async fn main() -> std::io::Result<()> {
///     web::server(async |_| {
///         web::App::new().service(
///             web::scope("/{project_id}")
///                 .service(web::resource("/path1").to(async || { web::HttpResponse::Ok() }))
///                 .service(web::resource("/path2").to(async || { web::HttpResponse::Ok() }))
///                 .service(web::resource("/path3").to(async || { web::HttpResponse::MethodNotAllowed() }))
///             )
///    })
///    .bind("127.0.0.1:59090", ntex::SharedCfg::default())?
///    .run()
///    .await
/// }
/// ```
///
/// In the above example, three routes get added:
///  * `/{project_id}/path1`
///  * `/{project_id}/path2`
///  * `/{project_id}/path3`
///
pub fn scope<St: State, In: 'static, T: IntoPattern>(path: T) -> Scope<St, In> {
    Scope::new(path)
}

/// Create *route* without configuration.
pub fn route<St: State, U: 'static>() -> Route<St, U> {
    Route::new()
}

/// Create *route* with `GET` method guard.
///
/// ```rust
/// use ntex::web;
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///        .route(web::get().to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `GET` route gets added:
///  * `/{project_id}`
///
pub fn get<St: State, U: 'static>() -> Route<St, U> {
    method(Method::GET)
}

/// Create *route* with `POST` method guard.
///
/// ```rust
/// use ntex::web;
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///         .route(web::post().to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `POST` route gets added:
///  * `/{project_id}`
///
pub fn post<St: State, U: 'static>() -> Route<St, U> {
    method(Method::POST)
}

/// Create *route* with `PUT` method guard.
///
/// ```rust
/// use ntex::web;
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///         .route(web::put().to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `PUT` route gets added:
///  * `/{project_id}`
///
pub fn put<St: State, U: 'static>() -> Route<St, U> {
    method(Method::PUT)
}

/// Create *route* with `PATCH` method guard.
///
/// ```rust
/// use ntex::web;
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///         .route(web::patch().to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `PATCH` route gets added:
///  * `/{project_id}`
///
pub fn patch<St: State, U: 'static>() -> Route<St, U> {
    method(Method::PATCH)
}

/// Create *route* with `DELETE` method guard.
///
/// ```rust
/// use ntex::web;
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///         .route(web::delete().to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `DELETE` route gets added:
///  * `/{project_id}`
///
pub fn delete<St: State, U: 'static>() -> Route<St, U> {
    method(Method::DELETE)
}

/// Create *route* with `HEAD` method guard.
///
/// ```rust
/// use ntex::web;
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///         .route(web::head().to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `HEAD` route gets added:
///  * `/{project_id}`
///
pub fn head<St: State, U: 'static>() -> Route<St, U> {
    method(Method::HEAD)
}

/// Create *route* with `QUERY` method guard.
///
/// ```rust
/// use ntex::web;
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///         .route(web::query().to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `QUERY` route gets added:
///  * `/{project_id}`
///
pub fn query<St: State, U: 'static>() -> Route<St, U> {
    method(Method::QUERY)
}

/// Create *route* and add method guard.
///
/// ```rust
/// use ntex::{http, web};
///
/// let app = web::App::default().service(
///     web::resource("/{project_id}")
///         .route(web::method(http::Method::GET).to(async || { web::HttpResponse::Ok() }))
/// );
/// ```
///
/// In the above example, one `GET` route gets added:
///  * `/{project_id}`
///
pub fn method<St: State, U: 'static>(method: Method) -> Route<St, U> {
    Route::default().method(method)
}

/// Create a new route and add handler.
///
/// ```rust
/// use ntex::web;
///
/// async fn index() -> web::HttpResponse {
///    web::HttpResponse::Ok().build()
/// }
///
/// web::App::default().service(
///     web::resource("/").route(web::to(index))
/// );
/// ```
pub fn to<St, U, F, Args>(handler: F) -> Route<St, U>
where
    St: State,
    U: 'static,
    F: Handler<St, Args> + 'static,
    Args: FromRequest<St> + 'static,
    Args::Error: WebResponseError<St, St::Error>,
{
    Route::new().to(handler)
}

/// Create service adapter for a specific path.
///
/// ```rust
/// use std::convert::Infallible;
/// use ntex::web::{self, guard, App, HttpResponse, WebError};
///
/// async fn my_service(req: web::WebRequest<()>) -> Result<web::WebResponse, Infallible> {
///     Ok(req.into_response(HttpResponse::Ok().build()))
/// }
///
/// let app = App::default().service(
///     web::service("/users/*")
///         .guard(guard::Header("content-type", "text/plain"))
///         .build(my_service)
/// );
/// ```
pub fn service<T: IntoPattern>(path: T) -> WebServiceAdapter {
    WebServiceAdapter::new(path)
}

/// Execute blocking function on a thread pool, returns future that resolves
/// to result of the function execution.
pub async fn block<F, I, E>(f: F) -> Result<I, BlockingError<E>>
where
    F: FnOnce() -> Result<I, E> + Send + Sync + 'static,
    I: Send + 'static,
    E: Send + fmt::Debug + 'static,
{
    match crate::rt::spawn_blocking(f).await {
        Ok(res) => res.map_err(BlockingError::Error),
        Err(_) => Err(BlockingError::Canceled),
    }
}

/// Create new http server with application factory.
///
/// ```rust,no_run
/// use ntex::{web, SharedCfg};
///
/// #[ntex::main]
/// async fn main() -> std::io::Result<()> {
///     web::server(async |_| {
///         web::App::new()
///             .service(web::resource("/").to(async || { web::HttpResponse::Ok() }))
///         })
///         .bind("127.0.0.1:59090", SharedCfg::default())?
///         .run()
///         .await
/// }
/// ```
pub fn server<F, I, Sf>(factory: F) -> HttpServer<NoConfig, F, I, Sf>
where
    F: AsyncFn(&()) -> I + Send + Clone + 'static,
    I: IntoServiceFactory<Sf, (), Request>,
    Sf: ServiceFactory<(), Request> + 'static,
    Sf::Res: Into<Response>,
    Sf::Error: ResponseError,
    Sf::InitError: IntoFailure,
{
    HttpServer::new(factory)
}

/// Create new http server with application factory and configuration.
///
/// ```rust,no_run
/// use std::io;
/// use ntex::{web, SharedCfg};
///
/// #[derive(Clone)]
/// struct AppState;
///
/// impl web::State for AppState {
///     type Error = web::DefaultError;
/// }
///
/// struct AppStateBuilder;
///
/// impl ntex::server::ServerAppConfig for AppStateBuilder {
///     type State = AppState;
///
///     async fn create(&self) -> io::Result<Self::State> {
///         Ok(AppState)
///     }
/// }
///
/// #[ntex::main]
/// async fn main() -> io::Result<()> {
///     web::server_with_config(AppStateBuilder, async |_| {
///         web::App::new()
///             .service(web::resource("/").to(async || { web::HttpResponse::Ok() }))
///         })
///         .bind("127.0.0.1:59090", SharedCfg::default())?
///         .run()
///         .await
/// }
/// ```
pub fn server_with_config<Cfg, F, I, Sf>(cfg: Cfg, factory: F) -> HttpServer<Cfg, F, I, Sf>
where
    Cfg: ServerAppConfig,
    F: AsyncFn(&Cfg::State) -> I + Send + Clone + 'static,
    I: IntoServiceFactory<Sf, Cfg::State, Request>,
    Sf: ServiceFactory<Cfg::State, Request> + 'static,
    Sf::Res: Into<Response>,
    Sf::Error: ResponseError,
    Sf::InitError: IntoFailure,
{
    HttpServer::with_config(cfg, factory)
}

struct Enc(ContentEncoding);

/// Helper trait that allows to set specific encoding for response.
pub trait BodyEncoding {
    /// Get content encoding
    fn get_encoding(&self) -> Option<ContentEncoding>;

    /// Set content encoding
    fn encoding(&mut self, encoding: ContentEncoding) -> &mut Self;
}

impl BodyEncoding for HttpResponseBuilder {
    fn get_encoding(&self) -> Option<ContentEncoding> {
        self.extensions().get::<Enc>().as_ref().map(|enc| enc.0)
    }

    fn encoding(&mut self, encoding: ContentEncoding) -> &mut Self {
        self.extensions_mut().insert(Enc(encoding));
        self
    }
}

impl<B> BodyEncoding for HttpResponse<B> {
    fn get_encoding(&self) -> Option<ContentEncoding> {
        self.extensions().get::<Enc>().as_ref().map(|enc| enc.0)
    }

    fn encoding(&mut self, encoding: ContentEncoding) -> &mut Self {
        self.extensions_mut().insert(Enc(encoding));
        self
    }
}
