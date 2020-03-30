//! Actix connect - tcp connector service
//!
//! ## Package feature
//!
//! * `openssl` - enables ssl support via `openssl` crate
//! * `rustls` - enables ssl support via `rustls` crate

mod connect;
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
pub use trust_dns_resolver::AsyncResolver;

pub mod resolver {
    pub use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
    pub use trust_dns_resolver::system_conf::read_system_conf;
    pub use trust_dns_resolver::{error::ResolveError, AsyncResolver};
}

use crate::rt::Arbiter;

pub use self::connect::{Address, Connect};
pub use self::error::ConnectError;
pub use self::resolve::Resolver;
pub use self::service::Connector;

pub fn start_resolver(cfg: ResolverConfig, opts: ResolverOpts) -> AsyncResolver {
    let (resolver, bg) = AsyncResolver::new(cfg, opts);
    crate::rt::spawn(bg);
    resolver
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

        let (resolver, bg) = AsyncResolver::new(cfg, opts);
        crate::rt::spawn(bg);

        Arbiter::set_item(DefaultResolver(resolver.clone()));
        resolver
    }
}
