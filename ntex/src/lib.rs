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

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use ntex_macros::{rt_main as main, rt_test as test};

#[cfg(test)]
pub(crate) use ntex_macros::rt_test2 as rt_test;

pub use ntex_service::{forward_poll_ready, forward_poll_shutdown};

pub mod http;
pub mod server;
pub mod web;
pub mod ws;

pub use self::service::{
    fn_service, into_service, pipeline, pipeline_factory, Container, Ctx, IntoService,
    IntoServiceFactory, Middleware, Service, ServiceCall, ServiceFactory,
};

pub use ntex_util::{channel, task};

pub mod codec {
    //! Utilities for encoding and decoding frames.
    pub use ntex_codec::*;
}

pub mod connect {
    //! Tcp connector service
    pub use ntex_connect::*;
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

    #[cfg(all(
        feature = "async-std",
        not(feature = "tokio"),
        not(feature = "glommio")
    ))]
    pub use ntex_async_std::*;

    #[cfg(all(
        feature = "glommio",
        not(feature = "tokio"),
        not(feature = "async-std")
    ))]
    pub use ntex_glommio::*;
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

pub mod util {
    pub use ntex_bytes::{
        Buf, BufMut, ByteString, Bytes, BytesMut, BytesVec, Pool, PoolId, PoolRef,
    };
    pub use ntex_util::{future::*, ready, services::*, HashMap, HashSet};
}
