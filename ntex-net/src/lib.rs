//! Utility for async runtime abstraction
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

mod compat;
pub mod connect;

pub use ntex_io::Io;
pub use ntex_rt::{spawn, spawn_blocking};

pub use self::compat::*;

#[cfg(all(
    feature = "default-rt",
    not(feature = "tokio"),
    not(feature = "async-std"),
    not(feature = "compio"),
    not(feature = "glommio")
))]
mod rt;
