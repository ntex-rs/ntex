use std::collections::VecDeque;
use std::marker::PhantomData;
use std::net::SocketAddr;

use actix_service::{NewService, Service};
use futures::future::{ok, Either, FutureResult};
use futures::{Async, Future, Poll};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::lookup_ip::LookupIpFuture;
use trust_dns_resolver::system_conf::read_system_conf;
use trust_dns_resolver::{AsyncResolver, Background};

use crate::connect::{Address, Connect};
use crate::error::ConnectError;

/// DNS Resolver Service factory
pub struct ResolverFactory<T> {
    resolver: AsyncResolver,
    _t: PhantomData<T>,
}

impl<T> Default for ResolverFactory<T> {
    fn default() -> Self {
        let (cfg, opts) = if let Ok((cfg, opts)) = read_system_conf() {
            (cfg, opts)
        } else {
            (ResolverConfig::default(), ResolverOpts::default())
        };

        ResolverFactory::new(cfg, opts)
    }
}

impl<T> ResolverFactory<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(cfg: ResolverConfig, opts: ResolverOpts) -> Self {
        let (resolver, bg) = AsyncResolver::new(cfg, opts);
        tokio_current_thread::spawn(bg);
        ResolverFactory {
            resolver,
            _t: PhantomData,
        }
    }
}

impl<T> Clone for ResolverFactory<T> {
    fn clone(&self) -> Self {
        ResolverFactory {
            resolver: self.resolver.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Address> NewService for ResolverFactory<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Service = Resolver<T>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(Resolver {
            resolver: self.resolver.clone(),
            _t: PhantomData,
        })
    }
}

/// DNS Resolver Service
pub struct Resolver<T> {
    resolver: AsyncResolver,
    _t: PhantomData<T>,
}

impl<T> Default for Resolver<T> {
    fn default() -> Self {
        let (cfg, opts) = if let Ok((cfg, opts)) = read_system_conf() {
            (cfg, opts)
        } else {
            (ResolverConfig::default(), ResolverOpts::default())
        };

        Resolver::new(cfg, opts)
    }
}

impl<T> Resolver<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(cfg: ResolverConfig, opts: ResolverOpts) -> Self {
        let (resolver, bg) = AsyncResolver::new(cfg, opts);
        tokio_current_thread::spawn(bg);
        Resolver {
            resolver,
            _t: PhantomData,
        }
    }
}

impl<T> Clone for Resolver<T> {
    fn clone(&self) -> Self {
        Resolver {
            resolver: self.resolver.clone(),
            _t: PhantomData,
        }
    }
}

impl<T: Address> Service for Resolver<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Future = Either<ResolverFuture<T>, FutureResult<Connect<T>, Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, mut req: Connect<T>) -> Self::Future {
        if req.addr.is_some() {
            Either::B(ok(req))
        } else {
            if let Ok(ip) = req.host().parse() {
                req.addr = Some(either::Either::Left(SocketAddr::new(ip, req.port())));
                Either::B(ok(req))
            } else {
                trace!("DNS resolver: resolving host {:?}", req.host());
                Either::A(ResolverFuture::new(req, &self.resolver))
            }
        }
    }
}

#[doc(hidden)]
/// Resolver future
pub struct ResolverFuture<T: Address> {
    req: Option<Connect<T>>,
    lookup: Option<Background<LookupIpFuture>>,
}

impl<T: Address> ResolverFuture<T> {
    pub fn new(req: Connect<T>, resolver: &AsyncResolver) -> Self {
        ResolverFuture {
            lookup: Some(resolver.lookup_ip(req.host())),
            req: Some(req),
        }
    }
}

impl<T: Address> Future for ResolverFuture<T> {
    type Item = Connect<T>;
    type Error = ConnectError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.lookup.as_mut().unwrap().poll().map_err(|e| {
            trace!(
                "DNS resolver: failed to resolve host {:?} err: {}",
                self.req.as_ref().unwrap().host(),
                e
            );
            e
        })? {
            Async::NotReady => Ok(Async::NotReady),
            Async::Ready(ips) => {
                let mut req = self.req.take().unwrap();
                let mut addrs: VecDeque<_> = ips
                    .iter()
                    .map(|ip| SocketAddr::new(ip, req.port()))
                    .collect();
                trace!(
                    "DNS resolver: host {:?} resolved to {:?}",
                    req.host(),
                    addrs
                );
                if addrs.len() == 1 {
                    req.addr = Some(either::Either::Left(addrs.pop_front().unwrap()));
                    Ok(Async::Ready(req))
                } else {
                    req.addr = Some(either::Either::Right(addrs));
                    Ok(Async::Ready(req))
                }
            }
        }
    }
}
