//! ntex - framework for composable network services
//!
//! ## Package feature
//!
//! * `openssl` - enables ssl support via `openssl` crate
//! * `rustls` - enables ssl support via `rustls` crate
//! * `compress` - enables compression support in http and web modules
//! * `cookie` - enables cookie support in http and web modules
#![deny(clippy::pedantic)]
#![allow(
    type_alias_bounds,
    missing_debug_implementations,
    clippy::cast_possible_truncation,
    clippy::missing_errors_doc,
    clippy::missing_fields_in_debug,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::too_many_lines,
    clippy::type_complexity,
    clippy::unused_async_trait_impl
)]
// Used for fake variadics
#![cfg_attr(any(docsrs, docsrs_dep), feature(rustdoc_internals))]

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use ntex_macros::{rt_main as main, rt_test as test};

#[cfg(test)]
pub(crate) use ntex_macros::rt_test_internal as rt_test;

pub use ntex_service::{forward_ready, forward_shutdown};

pub mod client;
pub mod http;
pub mod web;

#[cfg(feature = "ws")]
pub mod ws;

pub use self::service::{
    Ctx, IntoService, IntoServiceFactory, Middleware, Service, ServiceFactory, cfg::Cfg,
    cfg::SharedCfg, factory, fn_service, pipeline::Pipeline, pipeline::PipelineBinding,
    pipeline::PipelineFactory, svc,
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
        pub use ntex_tls::openssl::*;
    }

    #[cfg(feature = "rustls")]
    pub mod rustls {
        pub use ntex_tls::rustls::*;
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

    #[cfg(feature = "openssl")]
    pub use ntex_tls::openssl;

    #[cfg(feature = "rustls")]
    pub use ntex_tls::rustls;

    /// Ssl error combinded with service error.
    #[derive(Debug)]
    pub enum SslError<E> {
        Ssl(std::io::Error),
        Service(E),
    }
}

pub mod time {
    //! Utilities for tracking time.
    pub use ntex_util::time::*;
}

pub mod io {
    //! IO streaming utilities.
    pub use ntex_dispatcher::*;
    pub use ntex_io::*;
}

pub mod testing {
    //! IO testing utilities.
    pub use ntex_io::testing::IoTest;
}

pub mod tls {
    //! TLS support for ntex ecosystem.
    pub use ntex_tls::*;
}

pub mod error {
    pub use ntex_error::*;
}

pub mod util {
    pub use ntex_bytes::{Buf, BufMut, ByteString, Bytes, BytesMut};
    pub use ntex_bytes::{BytePage, BytePageSize, BytePages};
    pub use ntex_util::{
        HashMap, HashSet, clone_io_error, dyn_err, dyn_rc_err, future::*, services::*, str_rc_err,
    };

    #[doc(hidden)]
    pub fn enable_test_logging() {
        #[cfg(not(feature = "no-test-logging"))]
        if std::env::var("NTEX_NO_TEST_LOG").is_err() {
            if std::env::var("RUST_LOG").is_err() {
                unsafe {
                    std::env::set_var("RUST_LOG", "trace");
                }
            }
            let _ = env_logger::builder().is_test(true).try_init();
        }
    }
}
