//! Tcp connector service
use std::future::Future;

mod error;
mod message;
mod resolve;
mod service;
mod uri;

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

pub use trust_dns_resolver::config::{self, ResolverConfig, ResolverOpts};
use trust_dns_resolver::system_conf::read_system_conf;
pub use trust_dns_resolver::{error::ResolveError, TokioAsyncResolver as DnsResolver};

use crate::rt::{net::TcpStream, Arbiter};

pub use self::error::ConnectError;
pub use self::message::{Address, Connect};
pub use self::resolve::Resolver;
pub use self::service::Connector;

pub fn start_resolver(cfg: ResolverConfig, opts: ResolverOpts) -> DnsResolver {
    DnsResolver::tokio(cfg, opts).unwrap()
}

struct DefaultResolver(DnsResolver);

pub fn default_resolver() -> DnsResolver {
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

        let resolver = DnsResolver::tokio(cfg, opts).unwrap();

        Arbiter::set_item(DefaultResolver(resolver.clone()));
        resolver
    }
}

/// Resolve and connect to remote host
pub fn connect<T, U>(message: U) -> impl Future<Output = Result<TcpStream, ConnectError>>
where
    T: Address + 'static,
    Connect<T>: From<U>,
{
    service::ConnectServiceResponse::new(Box::pin(
        Resolver::new(default_resolver()).lookup(message.into()),
    ))
}
