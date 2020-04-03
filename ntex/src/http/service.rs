use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::{fmt, net};

use bytes::Bytes;
use futures::future::ok;
use futures::{ready, Future};
use h2::server::{self, Handshake};
use pin_project::pin_project;

use crate::codec::{AsyncRead, AsyncWrite, Framed};
use crate::rt::net::TcpStream;
use crate::service::{pipeline_factory, IntoServiceFactory, Service, ServiceFactory};

use super::body::MessageBody;
use super::builder::HttpServiceBuilder;
use super::config::{KeepAlive, ServiceConfig};
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
    _t: PhantomData<(T, B)>,
}

impl<T, S, B> HttpService<T, S, B>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
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
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
{
    /// Create new `HttpService` instance.
    pub fn new<F: IntoServiceFactory<S>>(service: F) -> Self {
        let cfg = ServiceConfig::new(KeepAlive::Timeout(5), 5000, 0, false, None);

        HttpService {
            cfg,
            srv: service.into_factory(),
            expect: h1::ExpectHandler,
            upgrade: None,
            on_connect: None,
            _t: PhantomData,
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
            _t: PhantomData,
        }
    }
}

impl<T, S, B, X, U> HttpService<T, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
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
    {
        HttpService {
            expect,
            cfg: self.cfg,
            srv: self.srv,
            upgrade: self.upgrade,
            on_connect: self.on_connect,
            _t: PhantomData,
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
            Request = (Request, Framed<T, h1::Codec>),
            Response = (),
        >,
        U1::Error: fmt::Display,
        U1::InitError: fmt::Debug,
    {
        HttpService {
            upgrade,
            cfg: self.cfg,
            srv: self.srv,
            expect: self.expect,
            on_connect: self.on_connect,
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

impl<S, B, X, U> HttpService<TcpStream, S, B, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    <X::Service as Service>::Future: 'static,
    U: ServiceFactory<
        Config = (),
        Request = (Request, Framed<TcpStream, h1::Codec>),
        Response = (),
    >,
    U::Error: fmt::Display + ResponseError,
    U::InitError: fmt::Debug,
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
        pipeline_factory(|io: TcpStream| {
            let peer_addr = io.peer_addr().ok();
            ok((io, Protocol::Http1, peer_addr))
        })
        .and_then(self)
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use super::*;
    use crate::server::openssl::{Acceptor, SslAcceptor, SslStream};
    use crate::server::{openssl::HandshakeError, SslError};

    impl<S, B, X, U> HttpService<SslStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
        <S::Service as Service>::Future: 'static,
        B: MessageBody + 'static,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError,
        X::InitError: fmt::Debug,
        <X::Service as Service>::Future: 'static,
        U: ServiceFactory<
            Config = (),
            Request = (Request, Framed<SslStream<TcpStream>, h1::Codec>),
            Response = (),
        >,
        U::Error: fmt::Display + ResponseError,
        U::InitError: fmt::Debug,
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
            Error = SslError<HandshakeError<TcpStream>, DispatchError>,
            InitError = (),
        > {
            pipeline_factory(
                Acceptor::new(acceptor)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: SslStream<TcpStream>| {
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
                ok((io, proto, peer_addr))
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
    use std::io;

    impl<S, B, X, U> HttpService<TlsStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
        <S::Service as Service>::Future: 'static,
        B: MessageBody + 'static,
        X: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X::Error: ResponseError,
        X::InitError: fmt::Debug,
        <X::Service as Service>::Future: 'static,
        U: ServiceFactory<
            Config = (),
            Request = (Request, Framed<TlsStream<TcpStream>, h1::Codec>),
            Response = (),
        >,
        U::Error: fmt::Display + ResponseError,
        U::InitError: fmt::Debug,
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
            Error = SslError<io::Error, DispatchError>,
            InitError = (),
        > {
            let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            config.set_protocols(&protos);

            pipeline_factory(
                Acceptor::new(config)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: TlsStream<TcpStream>| {
                let proto = if let Some(protos) = io.get_ref().1.get_alpn_protocol() {
                    if protos.windows(2).any(|window| window == b"h2") {
                        Protocol::Http2
                    } else {
                        Protocol::Http1
                    }
                } else {
                    Protocol::Http1
                };
                let peer_addr = io.get_ref().0.peer_addr().ok();
                ok((io, proto, peer_addr))
            })
            .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<T, S, B, X, U> ServiceFactory for HttpService<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    <X::Service as Service>::Future: 'static,
    U: ServiceFactory<
        Config = (),
        Request = (Request, Framed<T, h1::Codec>),
        Response = (),
    >,
    U::Error: fmt::Display + ResponseError,
    U::InitError: fmt::Debug,
    <U::Service as Service>::Future: 'static,
{
    type Config = ();
    type Request = (T, Protocol, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = HttpServiceHandler<T, S::Service, B, X::Service, U::Service>;
    type Future = HttpServiceResponse<T, S, B, X, U>;

    fn new_service(&self, _: ()) -> Self::Future {
        HttpServiceResponse {
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
#[pin_project]
pub struct HttpServiceResponse<
    T,
    S: ServiceFactory,
    B,
    X: ServiceFactory,
    U: ServiceFactory,
> {
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

impl<T, S, B, X, U> Future for HttpServiceResponse<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: ServiceFactory<Request = Request>,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Request = Request, Response = Request>,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    <X::Service as Service>::Future: 'static,
    U: ServiceFactory<Request = (Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display,
    U::InitError: fmt::Debug,
    <U::Service as Service>::Future: 'static,
{
    type Output =
        Result<HttpServiceHandler<T, S::Service, B, X::Service, U::Service>, ()>;

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
            HttpServiceHandler::new(
                this.cfg.clone(),
                service,
                this.expect.take().unwrap(),
                this.upgrade.take(),
                this.on_connect.clone(),
            )
        }))
    }
}

/// `Service` implementation for http transport
pub struct HttpServiceHandler<T, S: Service, B, X: Service, U: Service> {
    srv: Rc<S>,
    expect: Rc<X>,
    upgrade: Option<Rc<U>>,
    cfg: ServiceConfig,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    _t: PhantomData<(T, B, X)>,
}

impl<T, S: Service, B, X: Service, U: Service> HttpServiceHandler<T, S, B, X, U> {
    fn new(
        cfg: ServiceConfig,
        srv: S,
        expect: X,
        upgrade: Option<U>,
        on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    ) -> HttpServiceHandler<T, S, B, X, U> {
        HttpServiceHandler {
            cfg,
            on_connect,
            srv: Rc::new(srv),
            expect: Rc::new(expect),
            upgrade: upgrade.map(Rc::new),
            _t: PhantomData,
        }
    }
}

impl<T, S, B, X, U> Service for HttpServiceHandler<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display + ResponseError,
{
    type Request = (T, Protocol, Option<net::SocketAddr>);
    type Response = ();
    type Error = DispatchError;
    type Future = HttpServiceHandlerResponse<T, S, B, X, U>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready = self
            .expect
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
            })?
            .is_ready();

        let ready = self
            .srv
            .poll_ready(cx)
            .map_err(|e| {
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(Box::new(e))
            })?
            .is_ready()
            && ready;

        let ready = if let Some(ref upg) = self.upgrade {
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
        let ready = self.expect.poll_shutdown(cx, is_error).is_ready();
        let ready = self.srv.poll_shutdown(cx, is_error).is_ready() && ready;
        let ready = if let Some(ref upg) = self.upgrade {
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
        let on_connect = if let Some(ref on_connect) = self.on_connect {
            Some(on_connect(&io))
        } else {
            None
        };

        match proto {
            Protocol::Http2 => HttpServiceHandlerResponse {
                state: State::H2Handshake(Some((
                    server::handshake(io),
                    self.cfg.clone(),
                    self.srv.clone(),
                    on_connect,
                    peer_addr,
                ))),
            },
            Protocol::Http1 => HttpServiceHandlerResponse {
                state: State::H1(h1::Dispatcher::new(
                    io,
                    self.cfg.clone(),
                    self.srv.clone(),
                    self.expect.clone(),
                    self.upgrade.clone(),
                    on_connect,
                    peer_addr,
                )),
            },
        }
    }
}

#[pin_project]
pub struct HttpServiceHandlerResponse<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    #[pin]
    state: State<T, S, B, X, U>,
}

#[pin_project]
enum State<T, S, B, X, U>
where
    S: Service<Request = Request>,
    S::Error: ResponseError,
    T: AsyncRead + AsyncWrite + Unpin,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    H1(#[pin] h1::Dispatcher<T, S, B, X, U>),
    H2(Dispatcher<T, S, B>),
    H2Handshake(
        Option<(
            Handshake<T, Bytes>,
            ServiceConfig,
            Rc<S>,
            Option<Box<dyn DataFactory>>,
            Option<net::SocketAddr>,
        )>,
    ),
}

impl<T, S, B, X, U> Future for HttpServiceHandlerResponse<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request = Request>,
    S::Error: ResponseError,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError,
    U: Service<Request = (Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    type Output = Result<(), DispatchError>;

    #[pin_project::project]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        #[project]
        match this.state.project() {
            State::H1(disp) => disp.poll(cx),
            State::H2(ref mut disp) => Pin::new(disp).poll(cx),
            State::H2Handshake(ref mut data) => {
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
                let (_, cfg, srv, on_connect, peer_addr) = data.take().unwrap();
                self.as_mut().project().state.set(State::H2(Dispatcher::new(
                    srv, conn, on_connect, cfg, None, peer_addr,
                )));
                self.poll(cx)
            }
        }
    }
}
