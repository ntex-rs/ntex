use std::{
    cell::RefCell, error::Error, fmt, future::Future, marker, net, pin::Pin, rc::Rc,
    task,
};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::framed::State as IoState;
use crate::http::body::MessageBody;
use crate::http::config::{DispatcherConfig, OnRequest, ServiceConfig};
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
    on_request: RefCell<Option<OnRequest<T>>>,
    #[allow(dead_code)]
    handshake_timeout: u64,
    _t: marker::PhantomData<(T, B)>,
}

impl<T, S, B> H1Service<T, S, B>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
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
            on_request: RefCell::new(None),
            handshake_timeout: cfg.0.ssl_handshake_timeout,
            _t: marker::PhantomData,
            cfg,
        }
    }
}

impl<S, B, X, U> H1Service<TcpStream, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    S::Future: 'static,
    B: MessageBody,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    X::InitError: fmt::Debug,
    X::Future: 'static,
    U: ServiceFactory<
        Config = (),
        Request = (Request, TcpStream, IoState, Codec),
        Response = (),
    >,
    U::Error: fmt::Display + Error + 'static,
    U::InitError: fmt::Debug,
    U::Future: 'static,
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
        pipeline_factory(|io: TcpStream| async move {
            let peer_addr = io.peer_addr().ok();
            Ok((io, peer_addr))
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
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        S::Future: 'static,
        B: MessageBody,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError + 'static,
        X::InitError: fmt::Debug,
        X::Future: 'static,
        U: ServiceFactory<
            Config = (),
            Request = (Request, SslStream<TcpStream>, IoState, Codec),
            Response = (),
        >,
        U::Error: fmt::Display + Error + 'static,
        U::InitError: fmt::Debug,
        U::Future: 'static,
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
                    .timeout((self.handshake_timeout as u64) * 1000)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: SslStream<TcpStream>| async move {
                let peer_addr = io.get_ref().peer_addr().ok();
                Ok((io, peer_addr))
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
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        S::Future: 'static,
        B: MessageBody,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError + 'static,
        X::InitError: fmt::Debug,
        X::Future: 'static,
        U: ServiceFactory<
            Config = (),
            Request = (Request, TlsStream<TcpStream>, IoState, Codec),
            Response = (),
        >,
        U::Error: fmt::Display + Error + 'static,
        U::InitError: fmt::Debug,
        U::Future: 'static,
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
                    .timeout((self.handshake_timeout as u64) * 1000)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: TlsStream<TcpStream>| async move {
                let peer_addr = io.get_ref().0.peer_addr().ok();
                Ok((io, peer_addr))
            })
            .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<T, S, B, X, U> H1Service<T, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    S::Future: 'static,
    B: MessageBody,
{
    pub fn expect<X1>(self, expect: X1) -> H1Service<T, S, B, X1, U>
    where
        X1: ServiceFactory<Request = Request, Response = Request>,
        X1::Error: ResponseError + 'static,
        X1::InitError: fmt::Debug,
        X1::Future: 'static,
    {
        H1Service {
            expect,
            cfg: self.cfg,
            srv: self.srv,
            upgrade: self.upgrade,
            on_connect: self.on_connect,
            on_request: self.on_request,
            handshake_timeout: self.handshake_timeout,
            _t: marker::PhantomData,
        }
    }

    pub fn upgrade<U1>(self, upgrade: Option<U1>) -> H1Service<T, S, B, X, U1>
    where
        U1: ServiceFactory<Request = (Request, T, IoState, Codec), Response = ()>,
        U1::Error: fmt::Display + Error + 'static,
        U1::InitError: fmt::Debug,
        U1::Future: 'static,
    {
        H1Service {
            upgrade,
            cfg: self.cfg,
            srv: self.srv,
            expect: self.expect,
            on_connect: self.on_connect,
            on_request: self.on_request,
            handshake_timeout: self.handshake_timeout,
            _t: marker::PhantomData,
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

    /// Set req request callback.
    ///
    /// It get called once per request.
    pub(crate) fn on_request(self, f: Option<OnRequest<T>>) -> Self {
        *self.on_request.borrow_mut() = f;
        self
    }
}

impl<T, S, B, X, U> ServiceFactory for H1Service<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    S::Future: 'static,
    B: MessageBody,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    X::InitError: fmt::Debug,
    X::Future: 'static,
    U: ServiceFactory<
        Config = (),
        Request = (Request, T, IoState, Codec),
        Response = (),
    >,
    U::Error: fmt::Display + Error + 'static,
    U::InitError: fmt::Debug,
    U::Future: 'static,
{
    type Config = ();
    type Request = (T, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = H1ServiceHandler<T, S::Service, B, X::Service, U::Service>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let fut = self.srv.new_service(());
        let fut_ex = self.expect.new_service(());
        let fut_upg = self.upgrade.as_ref().map(|f| f.new_service(()));
        let on_connect = self.on_connect.clone();
        let on_request = self.on_request.borrow_mut().take();
        let cfg = self.cfg.clone();

        Box::pin(async move {
            let service = fut
                .await
                .map_err(|e| log::error!("Init http service error: {:?}", e))?;
            let expect = fut_ex
                .await
                .map_err(|e| log::error!("Init http service error: {:?}", e))?;
            let upgrade = if let Some(fut) = fut_upg {
                Some(
                    fut.await
                        .map_err(|e| log::error!("Init http service error: {:?}", e))?,
                )
            } else {
                None
            };

            let config = Rc::new(DispatcherConfig::new(
                cfg, service, expect, upgrade, on_request,
            ));

            Ok(H1ServiceHandler {
                config,
                on_connect,
                _t: marker::PhantomData,
            })
        })
    }
}

/// `Service` implementation for HTTP1 transport
pub struct H1ServiceHandler<T, S: Service, B, X: Service, U: Service> {
    config: Rc<DispatcherConfig<T, S, X, U>>,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    _t: marker::PhantomData<(T, B)>,
}

impl<T, S, B, X, U> Service for H1ServiceHandler<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<Request = (Request, T, IoState, Codec), Response = ()>,
    U::Error: fmt::Display + Error + 'static,
{
    type Request = (T, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type Future = Dispatcher<T, S, B, X, U>;

    fn poll_ready(
        &self,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), Self::Error>> {
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
                    DispatchError::Upgrade(Box::new(e))
                })?
                .is_ready()
                && ready
        } else {
            ready
        };

        if ready {
            task::Poll::Ready(Ok(()))
        } else {
            task::Poll::Pending
        }
    }

    fn poll_shutdown(
        &self,
        cx: &mut task::Context<'_>,
        is_error: bool,
    ) -> task::Poll<()> {
        let ready = self.config.expect.poll_shutdown(cx, is_error).is_ready();
        let ready = self.config.service.poll_shutdown(cx, is_error).is_ready() && ready;
        let ready = if let Some(ref upg) = self.config.upgrade {
            upg.poll_shutdown(cx, is_error).is_ready() && ready
        } else {
            ready
        };

        if ready {
            task::Poll::Ready(())
        } else {
            task::Poll::Pending
        }
    }

    fn call(&self, (io, addr): Self::Request) -> Self::Future {
        let on_connect = if let Some(ref on_connect) = self.on_connect {
            Some(on_connect(&io))
        } else {
            None
        };

        Dispatcher::new(io, self.config.clone(), addr, on_connect)
    }
}
