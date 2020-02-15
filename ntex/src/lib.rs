#![warn(
    rust_2018_idioms,
    // missing_debug_implementations,
    // missing_docs,
    // unreachable_pub,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::new_without_default,
    clippy::borrow_interior_mutable_const
)]

#[macro_use]
extern crate log;

pub mod http;
pub mod ws;
