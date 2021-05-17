//! Web framework for Rust.
//!
//! ```rust,no_run
//! use ntex::web;
//!
//! async fn index(info: web::types::Path<(String, u32)>) -> String {
//!     format!("Hello {}! id:{}", info.0, info.1)
//! }
//!
//! #[ntex::main]
//! async fn main() -> std::io::Result<()> {
//!     web::server(|| web::App::new().service(
//!         web::resource("/{name}/{id}/index.html").to(index))
//!     )
//!         .bind("127.0.0.1:8080")?
//!         .run()
//!         .await
//! }
//! ```
//!
//! ## Documentation & community resources
//!
//! Besides the API documentation (which you are currently looking
//! at!), several other resources are available:
//!
//! * [User Guide](https://docs.rs/ntex/)
//! * [GitHub repository](https://github.com/ntex-rs/ntex)
//! * [Cargo package](https://crates.io/crates/ntex)
//!
//! To get started navigating the API documentation you may want to
//! consider looking at the following pages:
//!
//! * [App](struct.App.html): This struct represents an ntex web
//!   application and is used to configure routes and other common
//!   settings.
//!
//! * [HttpServer](struct.HttpServer.html): This struct
//!   represents an HTTP server instance and is used to instantiate and
//!   configure servers.
//!
//! * [HttpRequest](struct.HttpRequest.html) and
//!   [HttpResponse](struct.HttpResponse.html): These structs
//!   represent HTTP requests and responses and expose various methods
//!   for inspecting, creating and otherwise utilizing them.
//!
//! ## Features
//!
//! * Supported *HTTP/1.x* and *HTTP/2.0* protocols
//! * Streaming and pipelining
//! * Keep-alive and slow requests handling
//! * *WebSockets* server/client
//! * Transparent content compression/decompression (br, gzip, deflate)
//! * Configurable request routing
//! * SSL support with OpenSSL or `rustls`
//! * Middlewares
//! * Supported Rust version: 1.41 or later
//!
//! ## Package feature
//!
//! * `cookie` - enables http cookie support
//! * `compress` - enables content encoding compression support
//! * `openssl` - enables ssl support via `openssl` crate
//! * `rustls` - enables ssl support via `rustls` crate

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
pub mod test;
pub mod types;
mod util;
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
pub use ntex_macros::web_trace as trace;

pub use crate::http::Response as HttpResponse;
pub use crate::http::ResponseBuilder as HttpResponseBuilder;

pub use self::app::App;
pub use self::config::ServiceConfig;
pub use self::error::{
    DefaultError, Error, ErrorContainer, ErrorRenderer, WebResponseError,
};
pub use self::extract::FromRequest;
pub use self::handler::Handler;
pub use self::httprequest::HttpRequest;
pub use self::request::WebRequest;
pub use self::resource::Resource;
pub use self::responder::Responder;
pub use self::response::WebResponse;
pub use self::route::Route;
pub use self::scope::Scope;
pub use self::server::HttpServer;
pub use self::service::WebServiceFactory;
pub use self::util::*;

pub mod dev {
    //! The `ntex::web` prelude for library developers
    //!
    //! The purpose of this module is to alleviate imports of many common
    //! traits by adding a glob import to the top of ntex::web heavy modules:

    use super::Handler;
    pub use crate::web::config::AppConfig;
    pub use crate::web::info::ConnectionInfo;
    pub use crate::web::request::WebRequest;
    pub use crate::web::response::WebResponse;
    pub use crate::web::rmap::ResourceMap;
    pub use crate::web::route::IntoRoutes;
    pub use crate::web::service::{
        WebServiceAdapter, WebServiceConfig, WebServiceFactory,
    };

    pub(crate) fn insert_slesh(mut patterns: Vec<String>) -> Vec<String> {
        for path in &mut patterns {
            if !path.is_empty() && !path.starts_with('/') {
                path.insert(0, '/');
            };
        }
        patterns
    }

    #[doc(hidden)]
    #[inline(always)]
    pub fn __assert_extractor<Err, T>()
    where
        T: super::FromRequest<Err>,
        Err: super::ErrorRenderer,
        <T as super::FromRequest<Err>>::Error: Into<Err::Container>,
    {
    }

    #[doc(hidden)]
    #[inline(always)]
    pub fn __assert_handler<Err, Fun, Fut>(
        f: Fun,
    ) -> impl Handler<(), Err, Future = Fut, Output = Fut::Output>
    where
        Err: super::ErrorRenderer,
        Fun: Fn() -> Fut + Clone + 'static,
        Fut: std::future::Future + 'static,
        Fut::Output: super::Responder<Err>,
    {
        f
    }

    macro_rules! assert_handler ({ $name:ident, $($T:ident),+} => {
        #[doc(hidden)]
        #[inline(always)]
        pub fn $name<Err, Fun, Fut, $($T,)+>(
            f: Fun,
        ) -> impl Handler<($($T,)+), Err, Future = Fut, Output = Fut::Output>
        where
            Err: $crate::web::ErrorRenderer,
            Fun: Fn($($T,)+) -> Fut + Clone + 'static,
            Fut: std::future::Future + 'static,
            Fut::Output: $crate::web::Responder<Err>,
        $($T: $crate::web::FromRequest<Err>),+,
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
