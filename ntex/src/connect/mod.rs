//! Tcp connector service

mod error;
mod message;
mod resolve;
mod service;
mod uri;

#[cfg(feature = "openssl")]
pub mod openssl;

#[cfg(feature = "rustls")]
pub mod rustls;

pub use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
pub use trust_dns_resolver::error::ResolveError;
use trust_dns_resolver::system_conf::read_system_conf;

use crate::rt::Arbiter;

pub use self::error::ConnectError;
pub use self::message::{Address, Connect};
pub use self::resolve::{AsyncResolver, Resolver};
pub use self::service::Connector;

pub fn start_resolver(cfg: ResolverConfig, opts: ResolverOpts) -> AsyncResolver {
    AsyncResolver::new(cfg, opts)
}

struct DefaultResolver(AsyncResolver);

pub fn default_resolver() -> AsyncResolver {
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

        let resolver = AsyncResolver::new(cfg, opts);

        Arbiter::set_item(DefaultResolver(resolver.clone()));
        resolver
    }
}
