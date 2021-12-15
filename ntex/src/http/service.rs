use std::{
    cell, error, fmt, future::Future, marker, net, pin::Pin, rc::Rc, task::Context,
    task::Poll,
};

use h2::server::{self, Handshake};

use crate::codec::{AsyncRead, AsyncWrite};
use crate::io::{types, DefaultFilter, Filter, Io, IoRef};
use crate::rt::net::TcpStream;
use crate::service::{pipeline_factory, IntoServiceFactory, Service, ServiceFactory};
use crate::time::{Millis, Seconds};
use crate::util::{Bytes, Pool, PoolId};

use super::body::MessageBody;
use super::builder::HttpServiceBuilder;
use super::config::{DispatcherConfig, KeepAlive, OnRequest, ServiceConfig};
use super::error::{DispatchError, ResponseError};
use super::helpers::DataFactory;
use super::request::Request;
use super::response::Response;
//use super::{h1, h2::Dispatcher, Protocol};
use super::{h1, Protocol};

/// `ServiceFactory` HTTP1.1/HTTP2 transport implementation
pub struct HttpService<F, S, B, X = h1::ExpectHandler, U = h1::UpgradeHandler<F>> {
    srv: S,
    cfg: ServiceConfig,
    expect: X,
    upgrade: Option<U>,
    on_request: cell::RefCell<Option<OnRequest>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B> HttpService<F, S, B>
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
    pub fn build() -> HttpServiceBuilder<F, S> {
        HttpServiceBuilder::new()
    }
}

impl<F, S, B> HttpService<F, S, B>
where
    F: Filter,
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
{
    /// Create new `HttpService` instance.
    pub fn new<U: IntoServiceFactory<S>>(service: U) -> Self {
        let cfg = ServiceConfig::new(
            KeepAlive::Timeout(Seconds(5)),
            Millis(5_000),
            Seconds::ZERO,
            Millis(5_000),
            PoolId::P1,
        );

        HttpService {
            cfg,
            srv: service.into_factory(),
            expect: h1::ExpectHandler,
            upgrade: None,
            on_request: cell::RefCell::new(None),
            _t: marker::PhantomData,
        }
    }

    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<U: IntoServiceFactory<S>>(
        cfg: ServiceConfig,
        service: U,
    ) -> Self {
        HttpService {
            cfg,
            srv: service.into_factory(),
            expect: h1::ExpectHandler,
            upgrade: None,
            on_request: cell::RefCell::new(None),
            _t: marker::PhantomData,
        }
    }
}

impl<F, S, B, X, U> HttpService<F, S, B, X, U>
where
    F: Filter,
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
    pub fn expect<X1>(self, expect: X1) -> HttpService<F, S, B, X1, U>
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
            on_request: self.on_request,
            _t: marker::PhantomData,
        }
    }

    /// Provide service for custom `Connection: UPGRADE` support.
    ///
    /// If service is provided then normal requests handling get halted
    /// and this service get called with original request and framed object.
    pub fn upgrade<U1>(self, upgrade: Option<U1>) -> HttpService<F, S, B, X, U1>
    where
        U1: ServiceFactory<
            Config = (),
            Request = (Request, Io<F>, h1::Codec),
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
            on_request: self.on_request,
            _t: marker::PhantomData,
        }
    }

    /// Set on request callback.
    pub(crate) fn on_request(self, f: Option<OnRequest>) -> Self {
        *self.on_request.borrow_mut() = f;
        self
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use super::*;
    use crate::server::openssl::{Acceptor, SslAcceptor, SslFilter};
    use crate::server::SslError;

    impl<S, B, X, U> HttpService<SslFilter<DefaultFilter>, S, B, X, U>
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
            Request = (Request, Io<SslFilter<DefaultFilter>>, h1::Codec),
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
            Request = Io<DefaultFilter>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
            pipeline_factory(
                Acceptor::new(acceptor)
                    .timeout(self.cfg.0.ssl_handshake_timeout)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(self.map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use super::*;
    use crate::server::rustls::{Acceptor, ServerConfig, TlsStream};
    use crate::server::SslError;

    impl<F, S, B, X, U> HttpService<F, S, B, X, U>
    where
        F: Filter,
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
            Request = (Request, Io<F>, h1::Codec),
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
            config: ServerConfig,
        ) -> impl ServiceFactory<
            Config = (),
            Request = Io<F>,
            Response = (),
            //Error = SslError<DispatchError>,
            Error = DispatchError,
            InitError = (),
        > {
            self
            // let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            // config.alpn_protocols = protos;

            // pipeline_factory(
            //     Acceptor::new(config)
            //         .timeout(self.cfg.0.ssl_handshake_timeout)
            //         .map_err(SslError::Ssl)
            //         .map_init_err(|_| panic!()),
            // )
            // .and_then(|io: TlsStream<TcpStream>| async move {
            //     let proto = io
            //         .get_ref()
            //         .1
            //         .alpn_protocol()
            //         .and_then(|protos| {
            //             if protos.windows(2).any(|window| window == b"h2") {
            //                 Some(Protocol::Http2)
            //             } else {
            //                 None
            //             }
            //         })
            //         .unwrap_or(Protocol::Http1);
            //     let peer_addr = io.get_ref().0.peer_addr().ok();
            //     Ok((io, proto, peer_addr))
            // })
            // .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, S, B, X, U> ServiceFactory for HttpService<F, S, B, X, U>
where
    F: Filter + 'static,
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
    U: ServiceFactory<Config = (), Request = (Request, Io<F>, h1::Codec), Response = ()>,
    U::Error: fmt::Display + error::Error + 'static,
    U::InitError: fmt::Debug,
    U::Future: 'static,
    <U::Service as Service>::Future: 'static,
{
    type Config = ();
    type Request = Io<F>;
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = HttpServiceHandler<F, S::Service, B, X::Service, U::Service>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let fut = self.srv.new_service(());
        let fut_ex = self.expect.new_service(());
        let fut_upg = self.upgrade.as_ref().map(|f| f.new_service(()));
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
            let pool = config.pool.into();

            Ok(HttpServiceHandler {
                pool,
                config: Rc::new(config),
                _t: marker::PhantomData,
            })
        })
    }
}

/// `Service` implementation for http transport
pub struct HttpServiceHandler<F, S: Service, B, X: Service, U: Service> {
    pool: Pool,
    config: Rc<DispatcherConfig<S, X, U>>,
    _t: marker::PhantomData<(F, B, X)>,
}

impl<F, S, B, X, U> Service for HttpServiceHandler<F, S, B, X, U>
where
    F: Filter + 'static,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<Request = (Request, Io<F>, h1::Codec), Response = ()>,
    U::Error: fmt::Display + error::Error + 'static,
{
    type Request = Io<F>;
    type Response = ();
    type Error = DispatchError;
    type Future = HttpServiceHandlerResponse<F, S, B, X, U>;

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

        let ready = self.pool.poll_ready(cx).is_ready() && ready;

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

    fn call(&self, io: Self::Request) -> Self::Future {
        log::trace!(
            "New http connection, peer address {:?}",
            io.query::<types::PeerAddr>().get()
        );

        //match proto {
        //Protocol::Http2 => todo!(),
        // HttpServiceHandlerResponse {
        //     state: ResponseState::H2Handshake {
        //         data: Some((
        //             server::Builder::new().handshake(io),
        //             self.config.clone(),
        //             on_connect,
        //             peer_addr,
        //         )),
        //     },
        // },
        // Protocol::Http1 =>
        HttpServiceHandlerResponse {
            state: ResponseState::H1 {
                fut: h1::Dispatcher::new(io, self.config.clone()),
            },
            // },
        }
    }
}

pin_project_lite::pin_project! {
    pub struct HttpServiceHandlerResponse<F, S, B, X, U>
    where
        F: Filter,
        F: 'static,
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
        U: Service<Request = (Request, Io<F>, h1::Codec), Response = ()>,
        U::Error: fmt::Display,
        U::Error: error::Error,
        U::Error: 'static,
    {
        #[pin]
        state: ResponseState<F, S, B, X, U>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum ResponseState<F, S, B, X, U>
    where
        S: Service<Request = Request>,
        S::Error: ResponseError,
        S::Error: 'static,
        F: Filter,
        F: 'static,
        B: MessageBody,
        X: Service<Request = Request, Response = Request>,
        X::Error: ResponseError,
        X::Error: 'static,
        U: Service<Request = (Request, Io<F>, h1::Codec), Response = ()>,
        U::Error: fmt::Display,
        U::Error: error::Error,
        U::Error: 'static,
    {
        H1 { #[pin] fut: h1::Dispatcher<F, S, B, X, U> },
        // H2 { fut: Dispatcher<F, S, B, X, U> },
        // H2Handshake { data:
        //     Option<(
        //         Handshake<T, Bytes>,
        //         Rc<DispatcherConfig<S, X, U>>,
        //         Option<Box<dyn DataFactory>>,
        //         Option<net::SocketAddr>,
        //     )>,
        // },
    }
}

impl<F, S, B, X, U> Future for HttpServiceHandlerResponse<F, S, B, X, U>
where
    F: Filter + 'static,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody,
    X: Service<Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<Request = (Request, Io<F>, h1::Codec), Response = ()>,
    U::Error: fmt::Display + error::Error + 'static,
{
    type Output = Result<(), DispatchError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        match this.state.project() {
            StateProject::H1 { fut } => fut.poll(cx),
            // StateProject::H2 { ref mut fut } => Pin::new(fut).poll(cx),
            // StateProject::H2Handshake { data } => {
            //     let conn = if let Some(ref mut item) = data {
            //         match Pin::new(&mut item.0).poll(cx) {
            //             Poll::Ready(Ok(conn)) => conn,
            //             Poll::Ready(Err(err)) => {
            //                 trace!("H2 handshake error: {}", err);
            //                 return Poll::Ready(Err(err.into()));
            //             }
            //             Poll::Pending => return Poll::Pending,
            //         }
            //     } else {
            //         panic!()
            //     };
            //     let (_, cfg, on_connect, peer_addr) = data.take().unwrap();
            //     self.as_mut().project().state.set(ResponseState::H2 {
            //         fut: Dispatcher::new(cfg, conn, on_connect, None, peer_addr),
            //     });
            //     self.poll(cx)
            // }
        }
    }
}
