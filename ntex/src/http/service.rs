use std::{
    cell, error, fmt, future::Future, marker, net, pin::Pin, rc::Rc, task::Context,
    task::Poll,
};

use bytes::Bytes;
use h2::server::{self, Handshake};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::framed::State;
use crate::rt::net::TcpStream;
use crate::service::{pipeline_factory, IntoServiceFactory, Service, ServiceFactory};

use super::body::MessageBody;
use super::builder::HttpServiceBuilder;
use super::config::{DispatcherConfig, KeepAlive, OnRequest, ServiceConfig};
use super::error::{DispatchError, ResponseError};
use super::helpers::DataFactory;
use super::request::Request;
use super::response::Response;
use super::{h1, h2::Dispatcher, Protocol};

/// `ServiceFactory` HTTP1.1/HTTP2 transport implementation
pub struct HttpService<T, S, B, X = h1::ExpectHandler, U = h1::UpgradeHandler<T>> {
    srv: S,
    cfg: ServiceConfig,
    expect: X,
    upgrade: Option<U>,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    on_request: cell::RefCell<Option<OnRequest<T>>>,
    _t: marker::PhantomData<(T, B)>,
}

impl<T, S, B> HttpService<T, S, B>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
{
    /// Create builder for `HttpService` instance.
    pub fn build() -> HttpServiceBuilder<T, S> {
        HttpServiceBuilder::new()
    }
}

impl<T, S, B> HttpService<T, S, B>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
{
    /// Create new `HttpService` instance.
    pub fn new<F: IntoServiceFactory<S>>(service: F) -> Self {
        let cfg = ServiceConfig::new(
            KeepAlive::Timeout(5),
            5000,
            0,
            5000,
            1024,
            8 * 1024,
            8 * 1024,
        );

        HttpService {
            cfg,
            srv: service.into_factory(),
            expect: h1::ExpectHandler,
            upgrade: None,
            on_connect: None,
            on_request: cell::RefCell::new(None),
            _t: marker::PhantomData,
        }
    }

    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<F: IntoServiceFactory<S>>(
        cfg: ServiceConfig,
        service: F,
    ) -> Self {
        HttpService {
            cfg,
            srv: service.into_factory(),
            expect: h1::ExpectHandler,
            upgrade: None,
            on_connect: None,
            on_request: cell::RefCell::new(None),
            _t: marker::PhantomData,
        }
    }
}

impl<T, S, B, X, U> HttpService<T, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody,
{
    /// Provide service for `EXPECT: 100-Continue` support.
    ///
    /// Service get called with request that contains `EXPECT` header.
    /// Service must return request in case of success, in that case
    /// request will be forwarded to main service.
    pub fn expect<X1>(self, expect: X1) -> HttpService<T, S, B, X1, U>
    where
        X1: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X1::Error: ResponseError,
        X1::InitError: fmt::Debug,
        X1::Future: 'static,
    {
        HttpService {
            expect,
            cfg: self.cfg,
            srv: self.srv,
            upgrade: self.upgrade,
            on_connect: self.on_connect,
            on_request: self.on_request,
            _t: marker::PhantomData,
        }
    }

    /// Provide service for custom `Connection: UPGRADE` support.
    ///
    /// If service is provided then normal requests handling get halted
    /// and this service get called with original request and framed object.
    pub fn upgrade<U1>(self, upgrade: Option<U1>) -> HttpService<T, S, B, X, U1>
    where
        U1: ServiceFactory<
            Config = (),
            Request = (Request, T, State, h1::Codec),
            Response = (),
        >,
        U1::Error: fmt::Display + error::Error + 'static,
        U1::InitError: fmt::Debug,
        U1::Future: 'static,
    {
        HttpService {
            upgrade,
            cfg: self.cfg,
            srv: self.srv,
            expect: self.expect,
            on_connect: self.on_connect,
            on_request: self.on_request,
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

    /// Set on request callback.
    pub(crate) fn on_request(self, f: Option<OnRequest<T>>) -> Self {
        *self.on_request.borrow_mut() = f;
        self
    }
}

impl<S, B, X, U> HttpService<TcpStream, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    X::InitError: fmt::Debug,
    X::Future: 'static,
    <X::Service as Service>::Future: 'static,
    U: ServiceFactory<
        Config = (),
        Request = (Request, TcpStream, State, h1::Codec),
        Response = (),
    >,
    U::Error: fmt::Display + error::Error + 'static,
    U::InitError: fmt::Debug,
    U::Future: 'static,
    <U::Service as Service>::Future: 'static,
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
            Ok((io, Protocol::Http1, peer_addr))
        })
        .and_then(self)
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use super::*;
    use crate::server::openssl::{Acceptor, SslAcceptor, SslStream};
    use crate::server::SslError;

    impl<S, B, X, U> HttpService<SslStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
        S::Future: 'static,
        <S::Service as Service>::Future: 'static,
        B: MessageBody + 'static,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError + 'static,
        X::InitError: fmt::Debug,
        X::Future: 'static,
        <X::Service as Service>::Future: 'static,
        U: ServiceFactory<
            Config = (),
            Request = (Request, SslStream<TcpStream>, State, h1::Codec),
            Response = (),
        >,
        U::Error: fmt::Display + error::Error + 'static,
        U::InitError: fmt::Debug,
        U::Future: 'static,
        <U::Service as Service>::Future: 'static,
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
                    .timeout((self.cfg.0.ssl_handshake_timeout as u64) * 1000)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: SslStream<TcpStream>| async move {
                let proto = if let Some(protos) = io.ssl().selected_alpn_protocol() {
                    if protos.windows(2).any(|window| window == b"h2") {
                        Protocol::Http2
                    } else {
                        Protocol::Http1
                    }
                } else {
                    Protocol::Http1
                };
                let peer_addr = io.get_ref().peer_addr().ok();
                Ok((io, proto, peer_addr))
            })
            .and_then(self.map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use super::*;
    use crate::server::rustls::{Acceptor, ServerConfig, Session, TlsStream};
    use crate::server::SslError;

    impl<S, B, X, U> HttpService<TlsStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Future: 'static,
        S::Response: Into<Response<B>> + 'static,
        <S::Service as Service>::Future: 'static,
        B: MessageBody + 'static,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError + 'static,
        X::InitError: fmt::Debug,
        X::Future: 'static,
        <X::Service as Service>::Future: 'static,
        U: ServiceFactory<
            Config = (),
            Request = (Request, TlsStream<TcpStream>, State, h1::Codec),
            Response = (),
        >,
        U::Error: fmt::Display + error::Error + 'static,
        U::InitError: fmt::Debug,
        U::Future: 'static,
        <U::Service as Service>::Future: 'static,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Config = (),
            Request = TcpStream,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            config.set_protocols(&protos);

            pipeline_factory(
                Acceptor::new(config)
                    .timeout((self.cfg.0.ssl_handshake_timeout as u64) * 1000)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: TlsStream<TcpStream>| async move {
                let proto = io
                    .get_ref()
                    .1
                    .get_alpn_protocol()
                    .and_then(|protos| {
                        if protos.windows(2).any(|window| window == b"h2") {
                            Some(Protocol::Http2)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(Protocol::Http1);
                let peer_addr = io.get_ref().0.peer_addr().ok();
                Ok((io, proto, peer_addr))
            })
            .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<T, S, B, X, U> ServiceFactory for HttpService<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    X::InitError: fmt::Debug,
    X::Future: 'static,
    <X::Service as Service>::Future: 'static,
    U: ServiceFactory<
        Config = (),
        Request = (Request, T, State, h1::Codec),
        Response = (),
    >,
    U::Error: fmt::Display + error::Error + 'static,
    U::InitError: fmt::Debug,
    U::Future: 'static,
    <U::Service as Service>::Future: 'static,
{
    type Config = ();
    type Request = (T, Protocol, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = HttpServiceHandler<T, S::Service, B, X::Service, U::Service>;
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

            let config =
                DispatcherConfig::new(cfg, service, expect, upgrade, on_request);

            Ok(HttpServiceHandler {
                on_connect,
                config: Rc::new(config),
                _t: marker::PhantomData,
            })
        })
    }
}

/// `Service` implementation for http transport
pub struct HttpServiceHandler<T, S: Service, B, X: Service, U: Service> {
    config: Rc<DispatcherConfig<T, S, X, U>>,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    _t: marker::PhantomData<(T, B, X)>,
}

impl<T, S, B, X, U> Service for HttpServiceHandler<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<Request = (Request, T, State, h1::Codec), Response = ()>,
    U::Error: fmt::Display + error::Error + 'static,
{
    type Request = (T, Protocol, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type Future = HttpServiceHandlerResponse<T, S, B, X, U>;

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
                    DispatchError::Upgrade(Box::new(e))
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

    fn call(&self, (io, proto, peer_addr): Self::Request) -> Self::Future {
        log::trace!(
            "New http connection protocol {:?} peer address {:?}",
            proto,
            peer_addr
        );
        let on_connect = if let Some(ref on_connect) = self.on_connect {
            Some(on_connect(&io))
        } else {
            None
        };

        match proto {
            Protocol::Http2 => HttpServiceHandlerResponse {
                state: ResponseState::H2Handshake {
                    data: Some((
                        server::handshake(io),
                        self.config.clone(),
                        on_connect,
                        peer_addr,
                    )),
                },
            },
            Protocol::Http1 => HttpServiceHandlerResponse {
                state: ResponseState::H1 {
                    fut: h1::Dispatcher::new(
                        io,
                        self.config.clone(),
                        peer_addr,
                        on_connect,
                    ),
                },
            },
        }
    }
}

pin_project_lite::pin_project! {
    pub struct HttpServiceHandlerResponse<T, S, B, X, U>
    where
        T: AsyncRead,
        T: AsyncWrite,
        T: Unpin,
        T: 'static,
        S: Service<Request = Request>,
        S::Error: ResponseError,
        S::Error: 'static,
        S::Response: Into<Response<B>>,
        S::Response: 'static,
        B: MessageBody,
        B: 'static,
        X: Service<Request = Request, Response = Request>,
        X::Error: ResponseError,
        X::Error: 'static,
        U: Service<Request = (Request, T, State, h1::Codec), Response = ()>,
        U::Error: fmt::Display,
        U::Error: error::Error,
        U::Error: 'static,
    {
        #[pin]
        state: ResponseState<T, S, B, X, U>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum ResponseState<T, S, B, X, U>
    where
        S: Service<Request = Request>,
        S::Error: ResponseError,
        S::Error: 'static,
        T: AsyncRead,
        T: AsyncWrite,
        T: Unpin,
        T: 'static,
        B: MessageBody,
        X: Service<Request = Request, Response = Request>,
        X::Error: ResponseError,
        X::Error: 'static,
        U: Service<Request = (Request, T, State, h1::Codec), Response = ()>,
        U::Error: fmt::Display,
        U::Error: error::Error,
        U::Error: 'static,
    {
        H1 { #[pin] fut: h1::Dispatcher<T, S, B, X, U> },
        H2 { fut: Dispatcher<T, S, B, X, U> },
        H2Handshake { data:
            Option<(
                Handshake<T, Bytes>,
                Rc<DispatcherConfig<T, S, X, U>>,
                Option<Box<dyn DataFactory>>,
                Option<net::SocketAddr>,
            )>,
        },
    }
}

impl<T, S, B, X, U> Future for HttpServiceHandlerResponse<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin + 'static,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<Request = (Request, T, State, h1::Codec), Response = ()>,
    U::Error: fmt::Display + error::Error + 'static,
{
    type Output = Result<(), DispatchError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match this.state.project() {
            StateProject::H1 { fut } => fut.poll(cx),
            StateProject::H2 { ref mut fut } => Pin::new(fut).poll(cx),
            StateProject::H2Handshake { data } => {
                let conn = if let Some(ref mut item) = data {
                    match Pin::new(&mut item.0).poll(cx) {
                        Poll::Ready(Ok(conn)) => conn,
                        Poll::Ready(Err(err)) => {
                            trace!("H2 handshake error: {}", err);
                            return Poll::Ready(Err(err.into()));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                } else {
                    panic!()
                };
                let (_, cfg, on_connect, peer_addr) = data.take().unwrap();
                self.as_mut().project().state.set(ResponseState::H2 {
                    fut: Dispatcher::new(cfg, conn, on_connect, None, peer_addr),
                });
                self.poll(cx)
            }
        }
    }
}
