use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::{fmt, net};

use futures::future::ok;
use futures::ready;

use crate::codec::{AsyncRead, AsyncWrite, Framed};
use crate::http::body::MessageBody;
use crate::http::config::{DispatcherConfig, ServiceConfig};
use crate::http::error::{DispatchError, ResponseError};
use crate::http::helpers::DataFactory;
use crate::http::request::Request;
use crate::http::response::Response;
use crate::rt::net::TcpStream;
use crate::{pipeline_factory, IntoServiceFactory, Service, ServiceFactory};

use super::codec::Codec;
use super::dispatcher::Dispatcher;
use super::{ExpectHandler, UpgradeHandler};

/// `ServiceFactory` implementation for HTTP1 transport
pub struct H1Service<T, S, B, X = ExpectHandler, U = UpgradeHandler<T>> {
    srv: S,
    cfg: ServiceConfig,
    expect: X,
    upgrade: Option<U>,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    handshake_timeout: u64,
    _t: PhantomData<(T, B)>,
}

impl<T, S, B> H1Service<T, S, B>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<F: IntoServiceFactory<S>>(
        cfg: ServiceConfig,
        service: F,
    ) -> Self {
        H1Service {
            srv: service.into_factory(),
            expect: ExpectHandler,
            upgrade: None,
            on_connect: None,
            handshake_timeout: cfg.0.ssl_handshake_timeout,
            _t: PhantomData,
            cfg,
        }
    }
}

impl<S, B, X, U> H1Service<TcpStream, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    U: ServiceFactory<
        Config = (),
        Request = (Request, Framed<TcpStream, Codec>),
        Response = (),
    >,
    U::Error: fmt::Display + ResponseError,
    U::InitError: fmt::Debug,
{
    /// Create simple tcp stream service
    pub fn tcp(
        self,
    ) -> impl ServiceFactory<
        Config = (),
        Request = TcpStream,
        Response = (),
        Error = DispatchError,
        InitError = (),
    > {
        pipeline_factory(|io: TcpStream| {
            let peer_addr = io.peer_addr().ok();
            ok((io, peer_addr))
        })
        .and_then(self)
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use super::*;

    use crate::server::openssl::{Acceptor, SslAcceptor, SslStream};
    use crate::server::SslError;

    impl<S, B, X, U> H1Service<SslStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError,
        X::InitError: fmt::Debug,
        U: ServiceFactory<
            Config = (),
            Request = (Request, Framed<SslStream<TcpStream>, Codec>),
            Response = (),
        >,
        U::Error: fmt::Display + ResponseError,
        U::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: SslAcceptor,
        ) -> impl ServiceFactory<
            Config = (),
            Request = TcpStream,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            pipeline_factory(
                Acceptor::new(acceptor)
                    .timeout(self.handshake_timeout)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: SslStream<TcpStream>| {
                let peer_addr = io.get_ref().peer_addr().ok();
                ok((io, peer_addr))
            })
            .and_then(self.map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use super::*;
    use crate::server::rustls::{Acceptor, ServerConfig, TlsStream};
    use crate::server::SslError;
    use std::fmt;

    impl<S, B, X, U> H1Service<TlsStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        B: MessageBody,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError,
        X::InitError: fmt::Debug,
        U: ServiceFactory<
            Config = (),
            Request = (Request, Framed<TlsStream<TcpStream>, Codec>),
            Response = (),
        >,
        U::Error: fmt::Display + ResponseError,
        U::InitError: fmt::Debug,
    {
        /// Create rustls based service
        pub fn rustls(
            self,
            config: ServerConfig,
        ) -> impl ServiceFactory<
            Config = (),
            Request = TcpStream,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            pipeline_factory(
                Acceptor::new(config)
                    .timeout(self.handshake_timeout)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: TlsStream<TcpStream>| {
                let peer_addr = io.get_ref().0.peer_addr().ok();
                ok((io, peer_addr))
            })
            .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<T, S, B, X, U> H1Service<T, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
{
    pub fn expect<X1>(self, expect: X1) -> H1Service<T, S, B, X1, U>
    where
        X1: ServiceFactory<Request = Request, Response = Request>,
        X1::Error: ResponseError,
        X1::InitError: fmt::Debug,
    {
        H1Service {
            expect,
            cfg: self.cfg,
            srv: self.srv,
            upgrade: self.upgrade,
            on_connect: self.on_connect,
            handshake_timeout: self.handshake_timeout,
            _t: PhantomData,
        }
    }

    pub fn upgrade<U1>(self, upgrade: Option<U1>) -> H1Service<T, S, B, X, U1>
    where
        U1: ServiceFactory<Request = (Request, Framed<T, Codec>), Response = ()>,
        U1::Error: fmt::Display,
        U1::InitError: fmt::Debug,
    {
        H1Service {
            upgrade,
            cfg: self.cfg,
            srv: self.srv,
            expect: self.expect,
            on_connect: self.on_connect,
            handshake_timeout: self.handshake_timeout,
            _t: PhantomData,
        }
    }

    /// Set on connect callback.
    pub(crate) fn on_connect(
        mut self,
        f: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    ) -> Self {
        self.on_connect = f;
        self
    }
}

impl<T, S, B, X, U> ServiceFactory for H1Service<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    U: ServiceFactory<Config = (), Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display + ResponseError,
    U::InitError: fmt::Debug,
{
    type Config = ();
    type Request = (T, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = H1ServiceHandler<T, S::Service, B, X::Service, U::Service>;
    type Future = H1ServiceResponse<T, S, B, X, U>;

    fn new_service(&self, _: ()) -> Self::Future {
        H1ServiceResponse {
            fut: self.srv.new_service(()),
            fut_ex: Some(self.expect.new_service(())),
            fut_upg: self.upgrade.as_ref().map(|f| f.new_service(())),
            expect: None,
            upgrade: None,
            on_connect: self.on_connect.clone(),
            cfg: self.cfg.clone(),
            _t: PhantomData,
        }
    }
}

#[doc(hidden)]
#[pin_project::pin_project]
pub struct H1ServiceResponse<T, S, B, X, U>
where
    S: ServiceFactory<Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    X: ServiceFactory<Request = Request, Response = Request>,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    U: ServiceFactory<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display,
    U::InitError: fmt::Debug,
{
    #[pin]
    fut: S::Future,
    #[pin]
    fut_ex: Option<X::Future>,
    #[pin]
    fut_upg: Option<U::Future>,
    expect: Option<X::Service>,
    upgrade: Option<U::Service>,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    cfg: ServiceConfig,
    _t: PhantomData<(T, B)>,
}

impl<T, S, B, X, U> Future for H1ServiceResponse<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: ServiceFactory<Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    B: MessageBody,
    X: ServiceFactory<Request = Request, Response = Request>,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    U: ServiceFactory<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display,
    U::InitError: fmt::Debug,
{
    type Output = Result<H1ServiceHandler<T, S::Service, B, X::Service, U::Service>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        if let Some(fut) = this.fut_ex.as_pin_mut() {
            let expect = ready!(fut
                .poll(cx)
                .map_err(|e| log::error!("Init http service error: {:?}", e)))?;
            this = self.as_mut().project();
            *this.expect = Some(expect);
            this.fut_ex.set(None);
        }

        if let Some(fut) = this.fut_upg.as_pin_mut() {
            let upgrade = ready!(fut
                .poll(cx)
                .map_err(|e| log::error!("Init http service error: {:?}", e)))?;
            this = self.as_mut().project();
            *this.upgrade = Some(upgrade);
            this.fut_ex.set(None);
        }

        let result = ready!(this
            .fut
            .poll(cx)
            .map_err(|e| log::error!("Init http service error: {:?}", e)));

        Poll::Ready(result.map(|service| {
            let this = self.as_mut().project();
            let cfg = this.cfg.clone();
            let config = DispatcherConfig::new(
                cfg,
                service,
                this.expect.take().unwrap(),
                this.upgrade.take(),
            );
            H1ServiceHandler::new(Rc::new(config), this.on_connect.clone())
        }))
    }
}

/// `Service` implementation for HTTP1 transport
pub struct H1ServiceHandler<T, S: Service, B, X: Service, U: Service> {
    config: Rc<DispatcherConfig<S, X, U>>,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    _t: PhantomData<(T, B)>,
}

impl<T, S: Service, B, X: Service, U: Service> H1ServiceHandler<T, S, B, X, U> {
    fn new(
        config: Rc<DispatcherConfig<S, X, U>>,
        on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    ) -> H1ServiceHandler<T, S, B, X, U> {
        H1ServiceHandler {
            config,
            on_connect,
            _t: PhantomData,
        }
    }
}

impl<T, S, B, X, U> Service for H1ServiceHandler<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, Codec>), Response = ()>,
    U::Error: fmt::Display + ResponseError,
{
    type Request = (T, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type Future = Dispatcher<T, S, B, X, U>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let cfg = self.config.as_ref();

        let ready = cfg
            .expect
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
            })?
            .is_ready();

        let ready = cfg
            .service
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
            })?
            .is_ready()
            && ready;

        let ready = if let Some(ref upg) = cfg.upgrade {
            upg.poll_ready(cx)
                .map_err(|e| {
                    log::error!("Http service readiness error: {:?}", e);
                    DispatchError::Service(Box::new(e))
                })?
                .is_ready()
                && ready
        } else {
            ready
        };

        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let ready = self.config.expect.poll_shutdown(cx, is_error).is_ready();
        let ready = self.config.service.poll_shutdown(cx, is_error).is_ready() && ready;
        let ready = if let Some(ref upg) = self.config.upgrade {
            upg.poll_shutdown(cx, is_error).is_ready() && ready
        } else {
            ready
        };

        if ready {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn call(&self, (io, addr): Self::Request) -> Self::Future {
        let on_connect = if let Some(ref on_connect) = self.on_connect {
            Some(on_connect(&io))
        } else {
            None
        };

        Dispatcher::new(self.config.clone(), io, addr, on_connect)
    }
}
