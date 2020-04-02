use std::cell::RefCell;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use futures::future::{ok, Either, FutureExt, LocalBoxFuture, Ready};
use futures::ready;

use trust_dns_proto::{error::ProtoError, Time};
use trust_dns_resolver::config::{ResolverConfig, ResolverOpts};
use trust_dns_resolver::error::ResolveError;
use trust_dns_resolver::lookup_ip::LookupIp;
use trust_dns_resolver::name_server::{
    GenericConnection, GenericConnectionProvider, RuntimeProvider, Spawn,
};
use trust_dns_resolver::AsyncResolver as TAsyncResolver;

use crate::channel::condition::{Condition, Waiter};
use crate::rt::net::{self, TcpStream};
use crate::service::{Service, ServiceFactory};

use super::{default_resolver, Address, Connect, ConnectError};

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
            state: Rc::new(RefCell::new(AsyncResolverState::New(Some(
                TAsyncResolver::new(config, options, Handle).boxed_local(),
            )))),
        }
    }

    /// Constructs a new Resolver with the system configuration.
    ///
    /// This will use `/etc/resolv.conf` on Unix OSes and the registry on Windows.
    pub fn from_system_conf() -> Self {
        AsyncResolver {
            state: Rc::new(RefCell::new(AsyncResolverState::New(Some(
                TokioAsyncResolver::from_system_conf(Handle).boxed_local(),
            )))),
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

type TokioAsyncResolver =
    TAsyncResolver<GenericConnection, GenericConnectionProvider<TokioRuntime>>;

enum AsyncResolverState {
    New(Option<LocalBoxFuture<'static, Result<TokioAsyncResolver, ResolveError>>>),
    Creating(Condition),
    Resolver(Box<TokioAsyncResolver>),
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
                    *this.state.borrow_mut() =
                        AsyncResolverState::Resolver(Box::new(resolver));
                }
                LookupIpState::Wait(ref mut waiter) => {
                    ready!(waiter.poll_waiter(cx));
                    this.fut = LookupIpState::Init;
                }
                LookupIpState::Init => {
                    let mut state = this.state.borrow_mut();
                    match &mut *state {
                        AsyncResolverState::New(ref mut fut) => {
                            this.fut = LookupIpState::Create(fut.take().unwrap());
                            *state = AsyncResolverState::Creating(Condition::default());
                        }
                        AsyncResolverState::Creating(ref cond) => {
                            this.fut = LookupIpState::Wait(cond.wait());
                        }
                        AsyncResolverState::Resolver(ref resolver) => {
                            let host = this.host.clone();
                            let resolver: TokioAsyncResolver = Clone::clone(resolver);

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

#[derive(Clone, Copy)]
struct Handle;

impl Spawn for Handle {
    fn spawn_bg<F>(&mut self, future: F)
    where
        F: Future<Output = Result<(), ProtoError>> + Send + 'static,
    {
        crate::rt::Arbiter::spawn(future.map(|_| ()));
    }
}

struct UdpSocket(net::UdpSocket);

#[derive(Clone)]
struct TokioRuntime;
impl RuntimeProvider for TokioRuntime {
    type Handle = Handle;
    type Tcp = AsyncIo02As03<TcpStream>;
    type Timer = TokioTime;
    type Udp = UdpSocket;
}

/// Conversion from `tokio::io::{AsyncRead, AsyncWrite}` to `std::io::{AsyncRead, AsyncWrite}`
struct AsyncIo02As03<T>(T);

use crate::codec::{AsyncRead as AsyncRead02, AsyncWrite as AsyncWrite02};
use futures::io::{AsyncRead, AsyncWrite};

impl<T> Unpin for AsyncIo02As03<T> {}
impl<R: AsyncRead02 + Unpin> AsyncRead for AsyncIo02As03<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<W: AsyncWrite02 + Unpin> AsyncWrite for AsyncIo02As03<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[async_trait::async_trait]
impl trust_dns_proto::tcp::Connect for AsyncIo02As03<TcpStream> {
    type Transport = AsyncIo02As03<TcpStream>;

    async fn connect(addr: SocketAddr) -> io::Result<Self::Transport> {
        TcpStream::connect(&addr).await.map(AsyncIo02As03)
    }
}

#[async_trait::async_trait]
impl trust_dns_proto::udp::UdpSocket for UdpSocket {
    async fn bind(addr: &SocketAddr) -> io::Result<Self> {
        net::UdpSocket::bind(addr).await.map(UdpSocket)
    }

    async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0.recv_from(buf).await
    }

    async fn send_to(&mut self, buf: &[u8], target: &SocketAddr) -> io::Result<usize> {
        self.0.send_to(buf, target).await
    }
}

/// New type which is implemented using tokio::time::{Delay, Timeout}
struct TokioTime;

#[async_trait::async_trait]
impl Time for TokioTime {
    async fn delay_for(duration: std::time::Duration) {
        tokio::time::delay_for(duration).await
    }

    async fn timeout<F: 'static + Future + Send>(
        duration: std::time::Duration,
        future: F,
    ) -> Result<F::Output, std::io::Error> {
        tokio::time::timeout(duration, future)
            .await
            .map_err(move |_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "future timed out")
            })
    }
}
