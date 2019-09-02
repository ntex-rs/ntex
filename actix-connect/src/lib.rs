//! Actix connect - tcp connector service
//!
//! ## Package feature
//!
//! * `ssl` - enables ssl support via `openssl` crate
//! * `rust-tls` - enables ssl support via `rustls` crate

#![recursion_limit = "128"]

#[macro_use]
extern crate log;

mod connect;
mod connector;
mod error;
mod resolver;
mod service;
pub mod ssl;

#[cfg(feature = "uri")]
mod uri;

pub use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
pub use trust_dns_resolver::system_conf::read_system_conf;
pub use trust_dns_resolver::{error::ResolveError, AsyncResolver};

pub use self::connect::{Address, Connect, Connection};
pub use self::connector::{TcpConnector, TcpConnectorFactory};
pub use self::error::ConnectError;
pub use self::resolver::{Resolver, ResolverFactory};
pub use self::service::{ConnectService, ConnectServiceFactory};

use actix_rt::Arbiter;
use actix_service::{NewService, Service, ServiceExt};
use tokio_tcp::TcpStream;

pub fn start_resolver(cfg: ResolverConfig, opts: ResolverOpts) -> AsyncResolver {
    let (resolver, bg) = AsyncResolver::new(cfg, opts);
    tokio_current_thread::spawn(bg);
    resolver
}

struct DefaultResolver(AsyncResolver);

pub(crate) fn get_default_resolver() -> AsyncResolver {
    if Arbiter::contains_item::<DefaultResolver>() {
        return Arbiter::get_item(|item: &DefaultResolver| item.0.clone());
    } else {
        let (cfg, opts) = match read_system_conf() {
            Ok((cfg, opts)) => (cfg, opts),
            Err(e) => {
                log::error!("TRust-DNS can not load system config: {}", e);
                (ResolverConfig::default(), ResolverOpts::default())
            }
        };

        let (resolver, bg) = AsyncResolver::new(cfg, opts);
        tokio_current_thread::spawn(bg);

        Arbiter::set_item(DefaultResolver(resolver.clone()));
        resolver
    }
}

pub fn start_default_resolver() -> AsyncResolver {
    get_default_resolver()
}

/// Create tcp connector service
pub fn new_connector<T: Address>(
    resolver: AsyncResolver,
) -> impl Service<Request = Connect<T>, Response = Connection<T, TcpStream>, Error = ConnectError>
         + Clone {
    Resolver::new(resolver).and_then(TcpConnector::new())
}

/// Create tcp connector service
pub fn new_connector_factory<T: Address>(
    resolver: AsyncResolver,
) -> impl NewService<
    Config = (),
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::new(resolver).and_then(TcpConnectorFactory::new())
}

/// Create connector service with default parameters
pub fn default_connector<T: Address>(
) -> impl Service<Request = Connect<T>, Response = Connection<T, TcpStream>, Error = ConnectError>
         + Clone {
    Resolver::default().and_then(TcpConnector::new())
}

/// Create connector service factory with default parameters
pub fn default_connector_factory<T: Address>() -> impl NewService<
    Config = (),
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    ResolverFactory::default().and_then(TcpConnectorFactory::new())
}
