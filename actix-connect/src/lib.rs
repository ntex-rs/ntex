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

pub use trust_dns_resolver::{error::ResolveError, AsyncResolver};

pub use self::connect::{Address, Connect, Connection};
pub use self::connector::{Connector, ConnectorFactory};
pub use self::error::ConnectError;
pub use self::resolver::{Resolver, ResolverFactory};

use actix_service::{NewService, Service, ServiceExt};
use tokio_tcp::TcpStream;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::system_conf::read_system_conf;

pub fn start_resolver(cfg: ResolverConfig, opts: ResolverOpts) -> AsyncResolver {
    let (resolver, bg) = AsyncResolver::new(cfg, opts);
    tokio_current_thread::spawn(bg);
    resolver
}

pub fn start_default_resolver() -> AsyncResolver {
    let (cfg, opts) = if let Ok((cfg, opts)) = read_system_conf() {
        (cfg, opts)
    } else {
        (ResolverConfig::default(), ResolverOpts::default())
    };

    let (resolver, bg) = AsyncResolver::new(cfg, opts);
    tokio_current_thread::spawn(bg);
    resolver
}

/// Create tcp connector service
pub fn new_connector<T: Address>(
    resolver: AsyncResolver,
) -> impl Service<Request = Connect<T>, Response = Connection<T, TcpStream>, Error = ConnectError>
         + Clone {
    Resolver::new(resolver).and_then(Connector::new())
}

/// Create tcp connector service
pub fn new_connector_factory<T: Address>(
    resolver: AsyncResolver,
) -> impl NewService<
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::new(resolver).and_then(ConnectorFactory::new())
}

/// Create connector service with default parameters
pub fn default_connector<T: Address>(
) -> impl Service<Request = Connect<T>, Response = Connection<T, TcpStream>, Error = ConnectError>
         + Clone {
    Resolver::new(start_default_resolver()).and_then(Connector::new())
}

/// Create connector service factory with default parameters
pub fn default_connector_factory<T: Address>() -> impl NewService<
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::new(start_default_resolver()).and_then(ConnectorFactory::new())
}
