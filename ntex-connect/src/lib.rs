//! Tcp connector service
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]

#[cfg(feature = "openssl")]
pub mod openssl {
    pub use ntex_tls::openssl::{SslConnector as Connector, SslFilter};
    pub use tls_openssl::ssl::{
        Error as SslError, HandshakeError, SslConnector, SslMethod,
    };
}

#[cfg(feature = "rustls")]
pub mod rustls {
    pub use ntex_tls::rustls::{TlsClientFilter, TlsConnector as Connector};
    pub use tls_rust::{pki_types::ServerName, ClientConfig};
}

pub use ntex_net::connect::{connect, Address, Connect, ConnectError, Connector, Resolver};

#[allow(unused_imports)]
#[doc(hidden)]
pub mod net {
    pub use ntex_net::*;
}
