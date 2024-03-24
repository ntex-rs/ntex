//! Utility for async runtime abstraction
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

mod compat;

pub use ntex_io::Io;
pub use ntex_rt::spawn;

pub use self::compat::*;
