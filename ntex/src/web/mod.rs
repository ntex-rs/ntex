//! Web application framework.
//!
//! ```rust,no_run
//! use ntex::{web, SharedCfg};
//!
//! async fn index(info: web::types::Path<(String, u32)>) -> String {
//!     format!("Hello {}! id:{}", info.0, info.1)
//! }
//!
//! #[ntex::main]
//! async fn main() -> std::io::Result<()> {
//!     web::server(async |_| web::App::new().service(
//!         web::resource("/{name}/{id}/index.html").to(index))
//!     )
//!         .bind("127.0.0.1:8080", SharedCfg::default())?
//!         .run()
//!         .await
//! }
//! ```
//!
//! ## Documentation and community resources
//!
//! Additional resources:
//!
//! - [User guide](https://ntex.rs)
//! - [GitHub repository](https://github.com/ntex-rs/ntex)
//! - [Cargo package](https://crates.io/crates/ntex)
//!
//! Useful API entry points include:
//!
//! - [`App`] configures routes, application state, and middleware.
//! - [`HttpServer`] creates and configures HTTP server instances.
//! - [`HttpRequest`] provides request metadata and application state.
//! - [`HttpResponse`] and [`WebResponse`] represent HTTP responses.
//! - [`resource()`] and [`route()`] configure resource and route matching.
//!
//! ## Features
//!
//! - HTTP/1.x and HTTP/2
//! - Streaming and pipelining
//! - Keep-alive connections and slow-request handling
//! - WebSocket clients and servers
//! - Transparent Brotli, gzip, and deflate content encoding
//! - Configurable request routing
//! - TLS through OpenSSL or rustls
//! - Composable middleware
//!
//! ## Crate features
//!
//! - `cookie` enables HTTP cookie support.
//! - `compress` enables content compression and decompression.
//! - `openssl` enables TLS support through OpenSSL.
//! - `rustls` enables TLS support through rustls.
//! - `url` enables URL generation and URL-aware request helpers.
//! - `ws` enables the [`ws`] module.
#![allow(clippy::unused_async_trait_impl, clippy::mismatching_type_param_order)]
mod app;
mod app_service;
mod config;
pub mod error;
mod error_default;
mod extract;
pub mod guard;
mod handler;
mod httprequest;
mod info;
pub mod middleware;
mod request;
mod resource;
mod responder;
mod response;
mod rmap;
mod route;
mod scope;
mod server;
mod service;
pub mod stack;
mod state;
pub mod test;
pub mod types;
mod util;

#[cfg(feature = "ws")]
pub mod ws;

// re-export proc macro
pub use ntex_macros::web_connect as connect;
pub use ntex_macros::web_delete as delete;
pub use ntex_macros::web_get as get;
pub use ntex_macros::web_head as head;
pub use ntex_macros::web_options as options;
pub use ntex_macros::web_patch as patch;
pub use ntex_macros::web_post as post;
pub use ntex_macros::web_put as put;
pub use ntex_macros::web_query as query;
pub use ntex_macros::web_trace as trace;

pub use crate::http::Response as HttpResponse;
pub use crate::http::ResponseBuilder as HttpResponseBuilder;

pub use self::app::{App, AppServices};
pub use self::config::{ServiceConfig, WebAppConfig};
pub use self::error::{DefaultError, InternalError, WebError, WebResponseError};
pub use self::extract::FromRequest;
pub use self::handler::{Handler, HandlerSt};
pub use self::httprequest::HttpRequest;
pub use self::request::WebRequest;
pub use self::resource::{Resource, ResourceServices};
pub use self::responder::Responder;
pub use self::response::WebResponse;
pub use self::route::Route;
pub use self::scope::{Scope, ScopeServices};
pub use self::server::HttpServer;
pub use self::service::WebServiceFactory;
pub use self::state::{AppState, State};
pub use self::util::*;

use crate::error::Failure;
use crate::service::boxed::{BoxService, BoxServiceFactory};

pub(crate) type HttpHandler<St: State, In> =
    BoxService<St, WebRequest<In>, WebResponse, WebError<St, St::Error>>;
pub(crate) type HttpService<St: State, In> =
    BoxServiceFactory<St, WebRequest<In>, WebResponse, WebError<St, St::Error>, Failure>;

pub mod dev {
    //! Internal web framework types commonly needed by library authors.
    //!
    //! Importing this module's contents can reduce repetitive imports in
    //! libraries that build abstractions on top of [`crate::web`].

    pub use crate::web::app_service::AppService;
    pub use crate::web::info::ConnectionInfo;
    pub use crate::web::rmap::ResourceMap;
    pub use crate::web::route::IntoRoutes;
    pub use crate::web::service::{WebServiceAdapter, WebServiceConfig, WebServiceFactory};

    pub type DefaultState = ();

    use crate::web::Handler;

    pub(crate) fn insert_slash(mut patterns: Vec<String>) -> Vec<String> {
        for path in &mut patterns {
            if !path.is_empty() && !path.starts_with('/') {
                path.insert(0, '/');
            }
        }
        patterns
    }

    #[doc(hidden)]
    #[inline]
    pub fn __assert_handler<St, In, Fun, Res>(f: Fun) -> impl Handler<St, (), Output = Res>
    where
        St: super::State,
        Fun: AsyncFn() -> Res + 'static,
        Res: super::Responder<St>,
    {
        f
    }

    macro_rules! assert_handler ({ $name:ident, $($T:ident),+} => {
        #[doc(hidden)]
        #[inline(always)]
        pub fn $name<St, Fun, Res, $($T,)+>(
            f: Fun,
        ) -> impl Handler<St, ($($T,)+), Output = Res>
        where
            St: $crate::web::State,
            Fun: AsyncFn($($T,)+) -> Res + 'static,
            Res: super::Responder<St> + 'static,
           $($T: $crate::web::FromRequest<St>),+,
        {
            f
        }
    });

    assert_handler!(__assert_handler1, A);
    assert_handler!(__assert_handler2, A, B);
    assert_handler!(__assert_handler3, A, B, C);
    assert_handler!(__assert_handler4, A, B, C, D);
    assert_handler!(__assert_handler5, A, B, C, D, E);
    assert_handler!(__assert_handler6, A, B, C, D, E, F);
    assert_handler!(__assert_handler7, A, B, C, D, E, F, G);
    assert_handler!(__assert_handler8, A, B, C, D, E, F, G, H);
    assert_handler!(__assert_handler9, A, B, C, D, E, F, G, H, I);
    assert_handler!(__assert_handler10, A, B, C, D, E, F, G, H, I, J);
}
