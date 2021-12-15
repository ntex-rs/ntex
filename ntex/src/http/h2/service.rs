use std::task::{Context, Poll};
use std::{future::Future, marker::PhantomData, net, pin::Pin, rc::Rc};

use h2::server::{self, Handshake};
use log::error;

use crate::http::body::MessageBody;
use crate::http::config::{DispatcherConfig, ServiceConfig};
use crate::http::error::{DispatchError, ResponseError};
use crate::http::helpers::DataFactory;
use crate::http::request::Request;
use crate::http::response::Response;
use crate::io::{Filter, Io, IoRef};
use crate::rt::net::TcpStream;
use crate::service::{
    fn_factory, fn_service, pipeline_factory, IntoServiceFactory, Service,
    ServiceFactory,
};
use crate::time::Millis;
use crate::util::Bytes;

use super::dispatcher::Dispatcher;

/// `ServiceFactory` implementation for HTTP2 transport
pub struct H2Service<F, S, B> {
    srv: S,
    cfg: ServiceConfig,
    #[allow(dead_code)]
    handshake_timeout: Millis,
    _t: PhantomData<(F, B)>,
}

impl<F, S, B> H2Service<F, S, B>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>> + 'static,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<U: IntoServiceFactory<S>>(
        cfg: ServiceConfig,
        service: U,
    ) -> Self {
        H2Service {
            srv: service.into_factory(),
            handshake_timeout: cfg.0.ssl_handshake_timeout,
            _t: PhantomData,
            cfg,
        }
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use crate::server::openssl::{Acceptor, SslAcceptor, SslStream};
    use crate::server::SslError;

    use super::*;
    use crate::service::{fn_factory, fn_service};

    impl<S, B> H2Service<SslStream<TcpStream>, S, B>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError + 'static,
        S::Response: Into<Response<B>> + 'static,
        S::Future: 'static,
        <S::Service as Service>::Future: 'static,
        B: MessageBody + 'static,
    {
        /// Create ssl based service
        pub fn openssl(
            self,
            acceptor: SslAcceptor,
        ) -> impl ServiceFactory<
            Config = (),
            Request = TcpStream,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = S::InitError,
        > {
            pipeline_factory(
                Acceptor::new(acceptor)
                    .timeout(self.handshake_timeout)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(fn_factory(|| async {
                Ok::<_, S::InitError>(fn_service(
                    |io: SslStream<TcpStream>| async move {
                        let peer_addr = io.get_ref().peer_addr().ok();
                        Ok((io, peer_addr))
                    },
                ))
            }))
            .and_then(self.map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use super::*;
    use crate::server::rustls::{Acceptor, ServerConfig, TlsStream};
    use crate::server::SslError;

    impl<S, B> H2Service<TlsStream<TcpStream>, S, B>
    where
        S: ServiceFactory<Config = (), Request = Request>,
        S::Error: ResponseError + 'static,
        S::Response: Into<Response<B>> + 'static,
        S::Future: 'static,
        <S::Service as Service>::Future: 'static,
        B: MessageBody + 'static,
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
            InitError = S::InitError,
        > {
            let protos = vec!["h2".to_string().into()];
            config.alpn_protocols = protos;

            pipeline_factory(
                Acceptor::new(config)
                    .timeout(self.handshake_timeout)
                    .map_err(SslError::Ssl)
                    .map_init_err(|_| panic!()),
            )
            .and_then(fn_factory(|| async {
                Ok::<_, S::InitError>(fn_service(
                    |io: TlsStream<TcpStream>| async move {
                        let peer_addr = io.get_ref().0.peer_addr().ok();
                        Ok((io, peer_addr))
                    },
                ))
            }))
            .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, S, B> ServiceFactory for H2Service<F, S, B>
where
    F: Filter,
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>> + 'static,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    B: MessageBody + 'static,
{
    type Config = ();
    type Request = Io<F>;
    type Response = ();
    type Error = DispatchError;
    type InitError = S::InitError;
    type Service = H2ServiceHandler<F, S::Service, B>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Service, Self::InitError>>>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let fut = self.srv.new_service(());
        let cfg = self.cfg.clone();

        Box::pin(async move {
            let service = fut.await?;
            let config = Rc::new(DispatcherConfig::new(cfg, service, (), None, None));

            Ok(H2ServiceHandler {
                config,
                _t: PhantomData,
            })
        })
    }
}

/// `Service` implementation for http/2 transport
pub struct H2ServiceHandler<F, S: Service, B> {
    config: Rc<DispatcherConfig<S, (), ()>>,
    _t: PhantomData<(F, B)>,
}

impl<F, S, B> Service for H2ServiceHandler<F, S, B>
where
    F: Filter,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
{
    type Request = Io<F>;
    type Response = ();
    type Error = DispatchError;
    type Future = H2ServiceHandlerResponse<F, S, B>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.config.service.poll_ready(cx).map_err(|e| {
            error!("Service readiness error: {:?}", e);
            DispatchError::Service(Box::new(e))
        })
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.config.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, io: Self::Request) -> Self::Future {
        // trace!("New http2 connection, peer address: {:?}", addr);

        H2ServiceHandlerResponse {
            state: State::Handshake(
                io.get_ref(),
                self.config.clone(),
                server::Builder::new().handshake(io),
            ),
        }
    }
}

enum State<F, S: Service<Request = Request>, B: MessageBody>
where
    F: Filter,
    S::Future: 'static,
{
    Incoming(Dispatcher<F, S, B, (), ()>),
    Handshake(
        IoRef,
        Rc<DispatcherConfig<S, (), ()>>,
        Handshake<Io<F>, Bytes>,
    ),
}

pub struct H2ServiceHandlerResponse<F, S, B>
where
    F: Filter,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
{
    state: State<F, S, B>,
}

impl<F, S, B> Future for H2ServiceHandlerResponse<F, S, B>
where
    F: Filter,
    S: Service<Request = Request>,
    S::Error: ResponseError + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody,
{
    type Output = Result<(), DispatchError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.state {
            State::Incoming(ref mut disp) => Pin::new(disp).poll(cx),
            State::Handshake(ref io, ref config, ref mut handshake) => {
                match Pin::new(handshake).poll(cx) {
                    Poll::Ready(Ok(conn)) => {
                        trace!("H2 handshake completed");
                        self.state = State::Incoming(Dispatcher::new(
                            io.clone(),
                            config.clone(),
                            conn,
                            None,
                        ));
                        self.poll(cx)
                    }
                    Poll::Ready(Err(err)) => {
                        trace!("H2 handshake error: {}", err);
                        Poll::Ready(Err(err.into()))
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}
