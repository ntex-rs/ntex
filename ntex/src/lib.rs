//! ntex - framework for composable network services
//!
//! ## Package feature
//!
//! * `openssl` - enables ssl support via `openssl` crate
//! * `rustls` - enables ssl support via `rustls` crate
//! * `compress` - enables compression support in http and web modules
//! * `cookie` - enables cookie support in http and web modules
#![warn(
    rust_2018_idioms,
    unreachable_pub,
    // missing_debug_implementations,
    // missing_docs,
)]
#![allow(
    type_alias_bounds,
    clippy::type_complexity,
    clippy::borrow_interior_mutable_const,
    clippy::needless_doctest_main,
    clippy::too_many_arguments,
    clippy::new_without_default,
    clippy::return_self_not_must_use
)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate derive_more;

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use ntex_macros::{rt_main as main, rt_test as test};

#[cfg(test)]
pub(crate) use ntex_macros::rt_test2 as rt_test;

pub mod connect;
pub mod http;
pub mod server;
pub mod util;
pub mod web;
pub mod ws;

pub use self::service::{
    apply_fn, boxed, fn_factory, fn_factory_with_config, fn_service, into_service,
    pipeline, pipeline_factory, IntoService, IntoServiceFactory, Service, ServiceFactory,
    Transform,
};

pub use futures_core::stream::Stream;
pub use futures_sink::Sink;
pub use ntex_util::channel;
pub use ntex_util::task;

pub mod codec {
    //! Utilities for encoding and decoding frames.
    pub use ntex_codec::*;
}

pub mod router {
    //! Resource path matching library.
    pub use ntex_router::*;
}

pub mod rt {
    //! A runtime implementation that runs everything on the current thread.
    pub use ntex_rt::*;

    #[cfg(feature = "tokio")]
    pub use ntex_tokio::*;

    #[cfg(all(not(feature = "tokio"), feature = "async-std"))]
    pub use ntex_async_std::*;
}

pub mod service {
    pub use ntex_service::*;
}

pub mod time {
    //! Utilities for tracking time.
    pub use ntex_util::time::*;
}

pub mod io {
    //! IO streaming utilities.
    pub use ntex_io::*;

    pub use ntex_tokio::TokioIoBoxed;
}

pub mod testing {
    //! IO testing utilities.
    #[doc(hidden)]
    pub use ntex_io::testing::IoTest as Io;
    pub use ntex_io::testing::IoTest;
}

pub mod tls {
    //! TLS support for ntex ecosystem.
    pub use ntex_tls::*;
}
