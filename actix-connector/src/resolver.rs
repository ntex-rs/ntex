use std::collections::VecDeque;
use std::net::SocketAddr;

use actix_service::{NewService, Service};
use futures::future::{ok, Either, FutureResult};
use futures::{Async, Future, Poll};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::lookup_ip::LookupIpFuture;
use trust_dns_resolver::system_conf::read_system_conf;
use trust_dns_resolver::{AsyncResolver, Background};

use crate::connect::Connect;
use crate::error::ConnectError;

/// DNS Resolver Service factory
pub struct ResolverFactory {
    resolver: AsyncResolver,
}

impl Default for ResolverFactory {
    fn default() -> Self {
        let (cfg, opts) = if let Ok((cfg, opts)) = read_system_conf() {
            (cfg, opts)
        } else {
            (ResolverConfig::default(), ResolverOpts::default())
        };

        ResolverFactory::new(cfg, opts)
    }
}

impl ResolverFactory {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(cfg: ResolverConfig, opts: ResolverOpts) -> Self {
        let (resolver, bg) = AsyncResolver::new(cfg, opts);
        tokio_current_thread::spawn(bg);
        ResolverFactory { resolver }
    }
}

impl Clone for ResolverFactory {
    fn clone(&self) -> Self {
        ResolverFactory {
            resolver: self.resolver.clone(),
        }
    }
}

impl NewService for ResolverFactory {
    type Request = Connect;
    type Response = Connect;
    type Error = ConnectError;
    type Service = Resolver;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(Resolver {
            resolver: self.resolver.clone(),
        })
    }
}

/// DNS Resolver Service
pub struct Resolver {
    resolver: AsyncResolver,
}

impl Default for Resolver {
    fn default() -> Self {
        let (cfg, opts) = if let Ok((cfg, opts)) = read_system_conf() {
            (cfg, opts)
        } else {
            (ResolverConfig::default(), ResolverOpts::default())
        };

        Resolver::new(cfg, opts)
    }
}

impl Resolver {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(cfg: ResolverConfig, opts: ResolverOpts) -> Self {
        let (resolver, bg) = AsyncResolver::new(cfg, opts);
        tokio_current_thread::spawn(bg);
        Resolver { resolver }
    }
}

impl Clone for Resolver {
    fn clone(&self) -> Self {
        Resolver {
            resolver: self.resolver.clone(),
        }
    }
}

impl Service for Resolver {
    type Request = Connect;
    type Response = Connect;
    type Error = ConnectError;
    type Future = Either<ResolverFuture, FutureResult<Connect, Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Connect) -> Self::Future {
        match req {
            Connect::Host { host, port } => {
                if let Ok(ip) = host.parse() {
                    Either::B(ok(Connect::Addr {
                        host: host,
                        addr: either::Either::Left(SocketAddr::new(ip, port)),
                    }))
                } else {
                    trace!("DNS resolver: resolving host {:?}", host);
                    Either::A(ResolverFuture::new(host, port, &self.resolver))
                }
            }
            other => Either::B(ok(other)),
        }
    }
}

#[doc(hidden)]
/// Resolver future
pub struct ResolverFuture {
    host: Option<String>,
    port: u16,
    lookup: Option<Background<LookupIpFuture>>,
}

impl ResolverFuture {
    pub fn new(host: String, port: u16, resolver: &AsyncResolver) -> Self {
        ResolverFuture {
            lookup: Some(resolver.lookup_ip(host.as_str())),
            host: Some(host),
            port,
        }
    }
}

impl Future for ResolverFuture {
    type Item = Connect;
    type Error = ConnectError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.lookup.as_mut().unwrap().poll().map_err(|e| {
            trace!(
                "DNS resolver: failed to resolve host {:?} err: {}",
                self.host.as_ref().unwrap(),
                e
            );
            e
        })? {
            Async::NotReady => Ok(Async::NotReady),
            Async::Ready(ips) => {
                let host = self.host.take().unwrap();
                let mut addrs: VecDeque<_> = ips
                    .iter()
                    .map(|ip| SocketAddr::new(ip, self.port))
                    .collect();
                trace!("DNS resolver: host {:?} resolved to {:?}", host, addrs);
                if addrs.len() == 1 {
                    Ok(Async::Ready(Connect::Addr {
                        addr: either::Either::Left(addrs.pop_front().unwrap()),
                        host,
                    }))
                } else {
                    Ok(Async::Ready(Connect::Addr {
                        addr: either::Either::Right(addrs),
                        host,
                    }))
                }
            }
        }
    }
}
