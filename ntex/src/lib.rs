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
    // missing_docs
)]
#![allow(
    type_alias_bounds,
    clippy::type_complexity,
    clippy::borrow_interior_mutable_const,
    clippy::needless_doctest_main,
    clippy::too_many_arguments,
    clippy::new_without_default,
    clippy::let_underscore_future
)]

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use ntex_macros::{rt_main as main, rt_test as test};

#[cfg(test)]
pub(crate) use ntex_macros::rt_test2 as rt_test;

pub use ntex_service::{forward_poll, forward_ready, forward_shutdown};

pub mod http;
pub mod web;

#[cfg(feature = "ws")]
pub mod ws;

pub use self::service::{
    chain, chain_factory, fn_service, IntoService, IntoServiceFactory, Middleware,
    Pipeline, Service, ServiceCtx, ServiceFactory,
};

pub use ntex_util::{channel, task};

pub mod codec {
    //! Utilities for encoding and decoding frames.
    pub use ntex_codec::*;
}

pub mod connect {
    //! Tcp connector service
    pub use ntex_net::connect::*;

    #[cfg(feature = "openssl")]
    pub mod openssl {
        pub use ntex_tls::openssl::{SslConnector, SslFilter};
    }

    #[cfg(feature = "rustls")]
    pub mod rustls {
        pub use ntex_tls::rustls::{TlsClientFilter, TlsConnector};
    }
}

pub mod router {
    //! Resource path matching library.
    pub use ntex_router::*;
}

pub mod rt {
    //! A runtime implementation that runs everything on the current thread.
    pub use ntex_rt::*;

    pub use ntex_net::*;
}

pub mod service {
    pub use ntex_service::*;
}

pub mod server {
    //! General purpose tcp server
    pub use ntex_server::net::*;

    pub use ntex_server::{signal, Signal};

    #[cfg(feature = "openssl")]
    pub use ntex_tls::openssl;

    #[cfg(feature = "rustls")]
    pub use ntex_tls::rustls;
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
    pub use ntex_util::{future::*, services::*, HashMap, HashSet};

    #[doc(hidden)]
    pub fn enable_test_logging() {
        #[cfg(not(feature = "no-test-logging"))]
        if std::env::var("NTEX_NO_TEST_LOG").is_err() {
            if std::env::var("RUST_LOG").is_err() {
                std::env::set_var("RUST_LOG", "trace");
            }
            let _ = env_logger::builder().is_test(true).try_init();
        }
    }
}
