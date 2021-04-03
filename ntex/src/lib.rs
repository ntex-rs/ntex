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
    clippy::new_without_default
)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate derive_more;

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use ntex_macros::{rt_main as main, rt_test as test};

#[cfg(test)]
pub(crate) use ntex_macros::rt_test2 as rt_test;

pub mod channel;
pub mod connect;
pub mod framed;
pub mod http;
pub mod server;
pub mod testing;
pub mod util;
pub mod web;
pub mod ws;

pub use self::service::*;

pub use futures_core::stream::Stream;
pub use futures_sink::Sink;
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
}

pub mod service {
    pub use ntex_service::*;
}
