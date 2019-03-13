//! Actix connect - tcp connector service
//!
//! ## Package feature
//!
//! * `ssl` - enables ssl support via `openssl` crate
//! * `rust-tls` - enables ssl support via `rustls` crate

#[macro_use]
extern crate log;

mod connect;
mod connector;
mod error;
mod resolver;
pub mod ssl;

pub use trust_dns_resolver::error::ResolveError;

pub use self::connect::{Address, Connect, Connection};
pub use self::connector::{Connector, ConnectorFactory};
pub use self::error::ConnectError;
pub use self::resolver::{Resolver, ResolverFactory};

use actix_service::{NewService, Service, ServiceExt};
use tokio_tcp::TcpStream;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};

/// Create tcp connector service
pub fn new_connector<T: Address>(
    cfg: ResolverConfig,
    opts: ResolverOpts,
) -> impl Service<Request = Connect<T>, Response = Connection<T, TcpStream>, Error = ConnectError>
         + Clone {
    Resolver::new(cfg, opts).and_then(Connector::new())
}

/// Create tcp connector service
pub fn new_connector_factory<T: Address>(
    cfg: ResolverConfig,
    opts: ResolverOpts,
) -> impl NewService<
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::new(cfg, opts).and_then(ConnectorFactory::new())
}

/// Create connector service with default parameters
pub fn default_connector<T: Address>(
) -> impl Service<Request = Connect<T>, Response = Connection<T, TcpStream>, Error = ConnectError>
         + Clone {
    Resolver::default().and_then(Connector::new())
}

/// Create connector service factory with default parameters
pub fn default_connector_factory<T: Address>() -> impl NewService<
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::default().and_then(ConnectorFactory::new())
}
