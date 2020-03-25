//! Actix connect - tcp connector service
//!
//! ## Package feature
//!
//! * `openssl` - enables ssl support via `openssl` crate
//! * `rustls` - enables ssl support via `rustls` crate

mod connect;
mod connector;
mod error;
mod resolve;
mod service;
mod uri;

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::system_conf::read_system_conf;
use trust_dns_resolver::AsyncResolver;

pub mod resolver {
    pub use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
    pub use trust_dns_resolver::system_conf::read_system_conf;
    pub use trust_dns_resolver::{error::ResolveError, AsyncResolver};
}

use crate::rt::{net::TcpStream, Arbiter};
use crate::service::{pipeline, pipeline_factory, Service, ServiceFactory};

pub use self::connect::{Address, Connect, Connection};
pub use self::connector::{TcpConnector, TcpConnectorFactory};
pub use self::error::ConnectError;
pub use self::resolve::{Resolver, ResolverFactory};
pub use self::service::{ConnectService, ConnectServiceFactory, TcpConnectService};

pub fn start_resolver(cfg: ResolverConfig, opts: ResolverOpts) -> AsyncResolver {
    let (resolver, bg) = AsyncResolver::new(cfg, opts);
    actix_rt::spawn(bg);
    resolver
}

struct DefaultResolver(AsyncResolver);

pub(crate) fn get_default_resolver() -> AsyncResolver {
    if Arbiter::contains_item::<DefaultResolver>() {
        Arbiter::get_item(|item: &DefaultResolver| item.0.clone())
    } else {
        let (cfg, opts) = match read_system_conf() {
            Ok((cfg, opts)) => (cfg, opts),
            Err(e) => {
                log::error!("TRust-DNS can not load system config: {}", e);
                (ResolverConfig::default(), ResolverOpts::default())
            }
        };

        let (resolver, bg) = AsyncResolver::new(cfg, opts);
        actix_rt::spawn(bg);

        Arbiter::set_item(DefaultResolver(resolver.clone()));
        resolver
    }
}

pub fn start_default_resolver() -> AsyncResolver {
    get_default_resolver()
}

/// Create tcp connector service
pub fn new_connector<T: Address + 'static>(
    resolver: AsyncResolver,
) -> impl Service<
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
> + Clone {
    pipeline(Resolver::new(resolver)).and_then(TcpConnector::new())
}

/// Create tcp connector service
pub fn new_connector_factory<T: Address + 'static>(
    resolver: AsyncResolver,
) -> impl ServiceFactory<
    Config = (),
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    pipeline_factory(ResolverFactory::new(resolver)).and_then(TcpConnectorFactory::new())
}

/// Create connector service with default parameters
pub fn default_connector<T: Address + 'static>() -> impl Service<
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
> + Clone {
    pipeline(Resolver::default()).and_then(TcpConnector::new())
}

/// Create connector service factory with default parameters
pub fn default_connector_factory<T: Address + 'static>() -> impl ServiceFactory<
    Config = (),
    Request = Connect<T>,
    Response = Connection<T, TcpStream>,
    Error = ConnectError,
    InitError = (),
> + Clone {
    pipeline_factory(ResolverFactory::default()).and_then(TcpConnectorFactory::new())
}
