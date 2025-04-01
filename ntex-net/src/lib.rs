//! Utility for async runtime abstraction
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]
#![allow(unused_variables, dead_code)]

mod compat;
pub mod connect;

pub use ntex_io::Io;
pub use ntex_rt::{spawn, spawn_blocking};

cfg_if::cfg_if! {
    if #[cfg(all(feature = "neon", target_os = "linux", feature = "io-uring"))] {
        #[path = "rt_uring/mod.rs"]
        mod rt_impl;
        pub use self::rt_impl::{
            from_tcp_stream, from_unix_stream, tcp_connect, tcp_connect_in, unix_connect,
            unix_connect_in, active_stream_ops
        };
    } else if #[cfg(all(unix, feature = "neon"))] {
        #[path = "rt_polling/mod.rs"]
        mod rt_impl;
        pub use self::rt_impl::{
            from_tcp_stream, from_unix_stream, tcp_connect, tcp_connect_in, unix_connect,
            unix_connect_in, active_stream_ops
        };
    } else {
        pub use self::compat::*;
    }
}

#[cfg(all(unix, feature = "neon"))]
mod helpers;
