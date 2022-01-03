use std::task::{Context, Poll};
use std::{future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use h2::server::{self, Handshake};

use crate::http::body::MessageBody;
use crate::http::config::{DispatcherConfig, ServiceConfig};
use crate::http::error::{DispatchError, ResponseError};
use crate::http::request::Request;
use crate::http::response::Response;
use crate::io::{types, Filter, Io, IoRef, TokioIoBoxed};
use crate::service::{IntoServiceFactory, Service, ServiceFactory};
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
    S: ServiceFactory<Request>,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<U: IntoServiceFactory<S, Request>>(
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
    use ntex_tls::openssl::{Acceptor, SslFilter};
    use tls_openssl::ssl::SslAcceptor;

    use crate::io::Filter;
    use crate::server::SslError;
    use crate::service::pipeline_factory;

    use super::*;

    impl<F, S, B> H2Service<SslFilter<F>, S, B>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        /// Create ssl based service
        pub fn openssl(
            self,
            acceptor: SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = S::InitError,
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
    use ntex_tls::rustls::{Acceptor, TlsFilter};
    use tls_rustls::ServerConfig;

    use super::*;
    use crate::{server::SslError, service::pipeline_factory};

    impl<F, S, B> H2Service<TlsFilter<F>, S, B>
    where
        F: Filter,
        S: ServiceFactory<Request> + 'static,
        S::Error: ResponseError,
        S::Response: Into<Response<B>>,
        B: MessageBody,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            Response = (),
            Error = SslError<DispatchError>,
            InitError = S::InitError,
        > {
            let protos = vec!["h2".to_string().into()];
            config.alpn_protocols = protos;

            pipeline_factory(
                Acceptor::from(config)
                    .timeout(self.handshake_timeout)
                    .map_err(|e| SslError::Ssl(Box::new(e)))
                    .map_init_err(|_| panic!()),
            )
            .and_then(self.map_err(SslError::Service))
        }
    }
}

impl<F, S, B> ServiceFactory<Io<F>> for H2Service<F, S, B>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
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
pub struct H2ServiceHandler<F, S: Service<Request>, B> {
    config: Rc<DispatcherConfig<S, (), ()>>,
    _t: PhantomData<(F, B)>,
}

impl<F, S, B> Service<Io<F>> for H2ServiceHandler<F, S, B>
where
    F: Filter,
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    type Response = ();
    type Error = DispatchError;
    type Future = H2ServiceHandlerResponse<S, B>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.config.service.poll_ready(cx).map_err(|e| {
            log::error!("Service readiness error: {:?}", e);
            DispatchError::Service(Box::new(e))
        })
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.config.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, io: Io<F>) -> Self::Future {
        log::trace!(
            "New http2 connection, peer address {:?}",
            io.query::<types::PeerAddr>().get()
        );
        io.set_disconnect_timeout(self.config.client_disconnect.into());

        H2ServiceHandlerResponse {
            state: State::Handshake(
                io.get_ref(),
                self.config.clone(),
                server::Builder::new().handshake(TokioIoBoxed::from(io)),
            ),
        }
    }
}

enum State<S: Service<Request>, B: MessageBody>
where
    S: 'static,
{
    Incoming(Dispatcher<S, B, (), ()>),
    Handshake(
        IoRef,
        Rc<DispatcherConfig<S, (), ()>>,
        Handshake<TokioIoBoxed, Bytes>,
    ),
}

pub struct H2ServiceHandlerResponse<S, B>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    state: State<S, B>,
}

impl<S, B> Future for H2ServiceHandlerResponse<S, B>
where
    S: Service<Request> + 'static,
    S::Error: ResponseError,
    S::Response: Into<Response<B>>,
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
