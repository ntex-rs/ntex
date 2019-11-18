use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use actix_service::{Service, ServiceFactory};
use futures::future::{ok, Either, Ready};
use trust_dns_resolver::lookup_ip::LookupIpFuture;
use trust_dns_resolver::{AsyncResolver, Background};

use crate::connect::{Address, Connect};
use crate::error::ConnectError;
use crate::get_default_resolver;

/// DNS Resolver Service factory
pub struct ResolverFactory<T> {
    resolver: Option<AsyncResolver>,
    _t: PhantomData<T>,
}

impl<T> ResolverFactory<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(resolver: AsyncResolver) -> Self {
        ResolverFactory {
            resolver: Some(resolver),
            _t: PhantomData,
        }
    }

    pub fn service(&self) -> Resolver<T> {
        Resolver {
            resolver: self.resolver.clone(),
            _t: PhantomData,
        }
    }
}

impl<T> Default for ResolverFactory<T> {
    fn default() -> Self {
        ResolverFactory {
            resolver: None,
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

impl<T: Address> ServiceFactory for ResolverFactory<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Config = ();
    type Service = Resolver<T>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: &()) -> Self::Future {
        ok(self.service())
    }
}

/// DNS Resolver Service
pub struct Resolver<T> {
    resolver: Option<AsyncResolver>,
    _t: PhantomData<T>,
}

impl<T> Resolver<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(resolver: AsyncResolver) -> Self {
        Resolver {
            resolver: Some(resolver),
            _t: PhantomData,
        }
    }
}

impl<T> Default for Resolver<T> {
    fn default() -> Self {
        Resolver {
            resolver: None,
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
    type Future = Either<ResolverFuture<T>, Ready<Result<Connect<T>, Self::Error>>>;

    fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: Connect<T>) -> Self::Future {
        if req.addr.is_some() {
            Either::Right(ok(req))
        } else if let Ok(ip) = req.host().parse() {
            req.addr = Some(either::Either::Left(SocketAddr::new(ip, req.port())));
            Either::Right(ok(req))
        } else {
            trace!("DNS resolver: resolving host {:?}", req.host());
            if self.resolver.is_none() {
                self.resolver = Some(get_default_resolver());
            }
            Either::Left(ResolverFuture::new(req, self.resolver.as_ref().unwrap()))
        }
    }
}

#[doc(hidden)]
/// Resolver future
pub struct ResolverFuture<T: Address> {
    req: Option<Connect<T>>,
    lookup: Background<LookupIpFuture>,
}

impl<T: Address> ResolverFuture<T> {
    pub fn new(req: Connect<T>, resolver: &AsyncResolver) -> Self {
        let lookup = if let Some(host) = req.host().splitn(2, ':').next() {
            resolver.lookup_ip(host)
        } else {
            resolver.lookup_ip(req.host())
        };

        ResolverFuture {
            lookup,
            req: Some(req),
        }
    }
}

impl<T: Address> Future for ResolverFuture<T> {
    type Output = Result<Connect<T>, ConnectError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = self.get_mut();

        match Pin::new(&mut this.lookup).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(ips)) => {
                let req = this.req.take().unwrap();
                let port = req.port();
                let req = req.set_addrs(ips.iter().map(|ip| SocketAddr::new(ip, port)));

                trace!(
                    "DNS resolver: host {:?} resolved to {:?}",
                    req.host(),
                    req.addrs()
                );

                if req.addr.is_none() {
                    Poll::Ready(Err(ConnectError::NoRecords))
                } else {
                    Poll::Ready(Ok(req))
                }
            }
            Poll::Ready(Err(e)) => {
                trace!(
                    "DNS resolver: failed to resolve host {:?} err: {}",
                    this.req.as_ref().unwrap().host(),
                    e
                );
                Poll::Ready(Err(e.into()))
            }
        }
    }
}
