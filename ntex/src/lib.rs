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

pub use ntex_rt_macros::{main, test};

pub mod channel;
pub mod codec;
pub mod connect;
pub mod framed;
pub mod http;
pub mod router;
pub mod rt;
pub mod server;
pub mod service;
pub mod task;
pub mod util;
pub mod web;
pub mod ws;

pub use self::service::*;
