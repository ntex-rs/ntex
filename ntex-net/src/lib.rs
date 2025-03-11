//! Utility for async runtime abstraction
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]
#![allow(unused_variables, dead_code)]

mod compat;
pub mod connect;

pub use ntex_io::Io;
pub use ntex_rt::{spawn, spawn_blocking};

pub use self::compat::*;

#[cfg(all(
    feature = "neon",
    not(feature = "neon-uring"),
    not(feature = "tokio"),
    not(feature = "compio")
))]
mod rt_polling;

#[cfg(all(
    feature = "neon-uring",
    not(feature = "neon"),
    not(feature = "tokio"),
    not(feature = "compio")
))]
mod rt_uring;
