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
    clippy::type_complexity
)]
// Used for fake variadics
#![cfg_attr(any(docsrs, docsrs_dep), feature(rustdoc_internals))]

#[cfg(not(test))] // Work around for rust-lang/rust#62127
pub use ntex_macros::{rt_main as main, rt_test as test};

#[cfg(test)]
pub(crate) use ntex_macros::rt_test_internal as rt_test;

pub use ntex_service::{forward_poll, forward_ready, forward_shutdown};

pub mod client;
pub mod http;
pub mod web;

#[cfg(feature = "ws")]
pub mod ws;

pub use self::service::{
    IntoService, IntoServiceFactory, Middleware, Pipeline, Service, ServiceCtx,
    ServiceFactory, cfg::Cfg, cfg::SharedCfg, chain, chain_factory, fn_service,
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

    pub use ntex_server::{Signal, signal};

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

pub mod util {
    use std::{error::Error, io, rc::Rc};

    pub use ntex_bytes::{Buf, BufMut, ByteString, Bytes, BytesMut};
    pub use ntex_util::{HashMap, HashSet, error::*, future::*, services::*};

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

    pub fn dyn_rc_error<T: Error + 'static>(err: T) -> Rc<dyn Error> {
        Rc::new(err)
    }

    pub fn str_rc_error(s: String) -> Rc<dyn Error> {
        #[derive(thiserror::Error, Debug)]
        #[error("{_0}")]
        struct StringError(String);

        Rc::new(StringError(s))
    }

    pub fn clone_io_error(err: &io::Error) -> io::Error {
        io::Error::new(err.kind(), format!("{err:?}"))
    }
}
