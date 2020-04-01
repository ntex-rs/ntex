use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::future::{ok, Either, FutureExt, LocalBoxFuture, Ready};
use futures::ready;
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::error::ResolveError;
use trust_dns_resolver::lookup_ip::LookupIp;
use trust_dns_resolver::TokioAsyncResolver;

use crate::channel::condition::{Condition, Waiter};
use crate::service::{Service, ServiceFactory};

use super::connect::{Address, Connect};
use super::default_resolver;
use super::error::ConnectError;

/// DNS Resolver Service
pub struct Resolver<T> {
    resolver: AsyncResolver,
    _t: PhantomData<T>,
}

impl<T> Resolver<T> {
    /// Create new resolver instance with custom configuration and options.
    pub fn new(resolver: AsyncResolver) -> Self {
        Resolver {
            resolver,
            _t: PhantomData,
        }
    }
}

impl<T: Address> Resolver<T> {
    /// Lookup ip addresses for provided host
    pub fn lookup(
        &self,
        mut req: Connect<T>,
    ) -> Either<ResolverFuture<T>, Ready<Result<Connect<T>, ConnectError>>> {
        if req.addr.is_some() {
            Either::Right(ok(req))
        } else if let Ok(ip) = req.host().parse() {
            req.addr = Some(either::Either::Left(SocketAddr::new(ip, req.port())));
            Either::Right(ok(req))
        } else {
            trace!("DNS resolver: resolving host {:?}", req.host());
            Either::Left(ResolverFuture::new(req, &self.resolver))
        }
    }
}

impl<T> Default for Resolver<T> {
    fn default() -> Resolver<T> {
        Resolver {
            resolver: default_resolver(),
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

impl<T: Address> ServiceFactory for Resolver<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Config = ();
    type Service = Resolver<T>;
    type InitError = ();
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.clone())
    }
}

impl<T: Address> Service for Resolver<T> {
    type Request = Connect<T>;
    type Response = Connect<T>;
    type Error = ConnectError;
    type Future = Either<ResolverFuture<T>, Ready<Result<Connect<T>, Self::Error>>>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, req: Connect<T>) -> Self::Future {
        self.lookup(req)
    }
}

#[doc(hidden)]
/// Resolver future
pub struct ResolverFuture<T: Address> {
    req: Option<Connect<T>>,
    lookup: LookupIpFuture,
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

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
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

#[derive(Clone)]
/// An asynchronous resolver for DNS.
pub struct AsyncResolver {
    state: Rc<RefCell<AsyncResolverState>>,
}

impl AsyncResolver {
    /// Construct a new `AsyncResolver` with the provided configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - configuration, name_servers, etc. for the Resolver
    /// * `options` - basic lookup options for the resolver
    pub fn new(config: ResolverConfig, options: ResolverOpts) -> Self {
        AsyncResolver {
            state: Rc::new(RefCell::new(AsyncResolverState::New(config, options))),
        }
    }

    /// Constructs a new Resolver with the system configuration.
    ///
    /// This will use `/etc/resolv.conf` on Unix OSes and the registry on Windows.
    pub fn from_system_conf() -> Self {
        AsyncResolver {
            state: Rc::new(RefCell::new(AsyncResolverState::NewFromSystem)),
        }
    }

    pub fn lookup_ip(&self, host: &str) -> LookupIpFuture {
        LookupIpFuture {
            host: host.to_string(),
            state: self.state.clone(),
            fut: LookupIpState::Init,
        }
    }
}

enum AsyncResolverState {
    New(ResolverConfig, ResolverOpts),
    NewFromSystem,
    Creating(Condition),
    Resolver(TokioAsyncResolver),
}

pub struct LookupIpFuture {
    host: String,
    state: Rc<RefCell<AsyncResolverState>>,
    fut: LookupIpState,
}

enum LookupIpState {
    Init,
    Create(LocalBoxFuture<'static, Result<TokioAsyncResolver, ResolveError>>),
    Wait(Waiter),
    Lookup(LocalBoxFuture<'static, Result<LookupIp, ResolveError>>),
}

impl Future for LookupIpFuture {
    type Output = Result<LookupIp, ResolveError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().get_mut();

        loop {
            match this.fut {
                LookupIpState::Lookup(ref mut fut) => return Pin::new(fut).poll(cx),
                LookupIpState::Create(ref mut fut) => {
                    let resolver = ready!(Pin::new(fut).poll(cx))?;
                    this.fut = LookupIpState::Init;
                    *this.state.borrow_mut() = AsyncResolverState::Resolver(resolver);
                }
                LookupIpState::Wait(ref mut waiter) => {
                    ready!(waiter.poll_waiter(cx));
                    this.fut = LookupIpState::Init;
                }
                LookupIpState::Init => {
                    let mut state = this.state.borrow_mut();
                    match &mut *state {
                        AsyncResolverState::New(config, options) => {
                            this.fut = LookupIpState::Create(
                                TokioAsyncResolver::tokio(
                                    config.clone(),
                                    options.clone(),
                                )
                                .boxed_local(),
                            );
                            *state = AsyncResolverState::Creating(Condition::default());
                        }
                        AsyncResolverState::NewFromSystem => {
                            this.fut = LookupIpState::Create(
                                TokioAsyncResolver::tokio_from_system_conf()
                                    .boxed_local(),
                            );
                            *state = AsyncResolverState::Creating(Condition::default());
                        }
                        AsyncResolverState::Creating(ref cond) => {
                            this.fut = LookupIpState::Wait(cond.wait());
                        }
                        AsyncResolverState::Resolver(ref resolver) => {
                            let host = this.host.clone();
                            let resolver = resolver.clone();

                            this.fut = LookupIpState::Lookup(
                                async move { resolver.lookup_ip(host.as_str()).await }
                                    .boxed_local(),
                            );
                        }
                    }
                }
            }
        }
    }
}
