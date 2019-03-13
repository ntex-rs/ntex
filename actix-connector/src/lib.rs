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

pub use self::connect::{Connect, Stream};
pub use self::connector::{Connector, ConnectorFactory};
pub use self::error::ConnectError;
pub use self::resolver::{Resolver, ResolverFactory};

use actix_service::{NewService, Service, ServiceExt};
use tokio_tcp::TcpStream;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};

/// Create tcp connector service
pub fn new_connector(
    cfg: ResolverConfig,
    opts: ResolverOpts,
) -> impl Service<Request = Connect, Response = Stream<TcpStream>, Error = ConnectError> + Clone
{
    Resolver::new(cfg, opts).and_then(Connector)
}

/// Create tcp connector service
pub fn new_connector_factory(
    cfg: ResolverConfig,
    opts: ResolverOpts,
) -> impl NewService<
    Request = Connect,
    Response = Stream<TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::new(cfg, opts).and_then(ConnectorFactory)
}

/// Create connector service with default parameters
pub fn default_connector(
) -> impl Service<Request = Connect, Response = Stream<TcpStream>, Error = ConnectError> + Clone
{
    Resolver::default().and_then(Connector)
}

/// Create connector service factory with default parameters
pub fn default_connector_factory() -> impl NewService<
    Request = Connect,
    Response = Stream<TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::default().and_then(ConnectorFactory)
}
