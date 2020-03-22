#![warn(
    rust_2018_idioms,
    unreachable_pub,
    // missing_debug_implementations,
    // missing_docs,
)]
#![allow(
    clippy::type_complexity,
    clippy::borrow_interior_mutable_const,
    clippy::needless_doctest_main,
    clippy::too_many_arguments,
    clippy::new_without_default
)]

#[macro_use]
extern crate log;

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use actix_macros::{main, test};

pub mod channel;
pub mod framed;
pub mod http;
pub mod router;
pub mod server;
pub mod service;
pub mod task;
pub mod util;
pub mod web;
pub mod ws;

pub use self::service::*;
