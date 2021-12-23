use std::{
    cell::RefCell, error::Error, fmt, future::Future, marker, pin::Pin, rc::Rc, task,
};

use crate::http::body::MessageBody;
use crate::http::config::{DispatcherConfig, OnRequest, ServiceConfig};
use crate::http::error::{DispatchError, ResponseError};
use crate::http::request::Request;
use crate::http::response::Response;
use crate::io::{types, Filter, Io};
use crate::service::{IntoServiceFactory, Service, ServiceFactory};
use crate::time::Millis;

use super::codec::Codec;
use super::dispatcher::Dispatcher;
use super::{ExpectHandler, UpgradeHandler};

/// `ServiceFactory` implementation for HTTP1 transport
pub struct H1Service<F, S, B, X = ExpectHandler, U = UpgradeHandler<F>> {
    srv: S,
    cfg: ServiceConfig,
    expect: X,
    upgrade: Option<U>,
    on_request: RefCell<Option<OnRequest>>,
    #[allow(dead_code)]
    handshake_timeout: Millis,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B> H1Service<F, S, B>
where
    S: ServiceFactory<Request, Config = ()>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>>,
    B: MessageBody,
{
    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<U: IntoServiceFactory<S, Request>>(
        cfg: ServiceConfig,
        service: U,
    ) -> Self {
        H1Service {
            srv: service.into_factory(),
            expect: ExpectHandler,
            upgrade: None,
            on_request: RefCell::new(None),
            handshake_timeout: cfg.0.ssl_handshake_timeout,
            _t: marker::PhantomData,
            cfg,
        }
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use ntex_tls::openssl::{Acceptor, SslFilter};
    use tls_openssl::ssl::SslAcceptor;

    use super::*;
    use crate::{server::SslError, service::pipeline_factory};

    impl<F, S, B, X, U> H1Service<SslFilter<F>, S, B, X, U>
    where
        F: Filter,
        S: ServiceFactory<Request, Config = ()>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        S::Future: 'static,
        B: MessageBody,
        X: ServiceFactory<Request, Config = (), Response = Request>,
        X::Error: ResponseError + 'static,
        X::InitError: fmt::Debug,
        X::Future: 'static,
        U: ServiceFactory<
                (Request, Io<SslFilter<F>>, Codec),
                Config = (),
                Response = (),
            > + 'static,
        U::Error: fmt::Display + Error,
        U::InitError: fmt::Debug,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: SslAcceptor,
        ) -> impl ServiceFactory<
            Io<F>,
            Config = (),
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
            .and_then(self.map_err(SslError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use std::fmt;

    use ntex_tls::rustls::{Acceptor, TlsFilter};
    use tls_rustls::ServerConfig;

    use super::*;
    use crate::{server::SslError, service::pipeline_factory};

    impl<F, S, B, X, U> H1Service<TlsFilter<F>, S, B, X, U>
    where
        F: Filter,
        S: ServiceFactory<Request, Config = ()>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        S::Future: 'static,
        B: MessageBody,
        X: ServiceFactory<Request, Config = (), Response = Request>,
        X::Error: ResponseError + 'static,
        X::InitError: fmt::Debug,
        X::Future: 'static,
        U: ServiceFactory<
                (Request, Io<TlsFilter<F>>, Codec),
                Config = (),
                Response = (),
            > + 'static,
        U::Error: fmt::Display + Error,
        U::InitError: fmt::Debug,
    {
        /// Create rustls based service
        pub fn rustls(
            self,
            config: ServerConfig,
        ) -> impl ServiceFactory<
            Io<F>,
            Config = (),
            Response = (),
            Error = SslError<DispatchError>,
            InitError = (),
        > {
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

impl<F, S, B, X, U> H1Service<F, S, B, X, U>
where
    F: Filter,
    S: ServiceFactory<Request, Config = ()>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    S::Future: 'static,
    B: MessageBody,
{
    pub fn expect<X1>(self, expect: X1) -> H1Service<F, S, B, X1, U>
    where
        X1: ServiceFactory<Request, Response = Request>,
        X1::Error: ResponseError + 'static,
        X1::InitError: fmt::Debug,
        X1::Future: 'static,
    {
        H1Service {
            expect,
            cfg: self.cfg,
            srv: self.srv,
            upgrade: self.upgrade,
            on_request: self.on_request,
            handshake_timeout: self.handshake_timeout,
            _t: marker::PhantomData,
        }
    }

    pub fn upgrade<U1>(self, upgrade: Option<U1>) -> H1Service<F, S, B, X, U1>
    where
        U1: ServiceFactory<(Request, Io<F>, Codec), Response = ()>,
        U1::Error: fmt::Display + Error + 'static,
        U1::InitError: fmt::Debug,
        U1::Future: 'static,
    {
        H1Service {
            upgrade,
            cfg: self.cfg,
            srv: self.srv,
            expect: self.expect,
            on_request: self.on_request,
            handshake_timeout: self.handshake_timeout,
            _t: marker::PhantomData,
        }
    }

    /// Set req request callback.
    ///
    /// It get called once per request.
    pub(crate) fn on_request(self, f: Option<OnRequest>) -> Self {
        *self.on_request.borrow_mut() = f;
        self
    }
}

impl<F, S, B, X, U> ServiceFactory<Io<F>> for H1Service<F, S, B, X, U>
where
    F: Filter + 'static,
    S: ServiceFactory<Request, Config = ()>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    S::InitError: fmt::Debug,
    S::Future: 'static,
    B: MessageBody,
    X: ServiceFactory<Request, Config = (), Response = Request>,
    X::Error: ResponseError + 'static,
    X::InitError: fmt::Debug,
    X::Future: 'static,
    U: ServiceFactory<(Request, Io<F>, Codec), Config = (), Response = ()> + 'static,
    U::Error: fmt::Display + Error,
    U::InitError: fmt::Debug,
{
    type Config = ();
    type Response = ();
    type Error = DispatchError;
    type InitError = ();
    type Service = H1ServiceHandler<F, S::Service, B, X::Service, U::Service>;
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

            let config = Rc::new(DispatcherConfig::new(
                cfg, service, expect, upgrade, on_request,
            ));

            Ok(H1ServiceHandler {
                config,
                _t: marker::PhantomData,
            })
        })
    }
}

/// `Service` implementation for HTTP1 transport
pub struct H1ServiceHandler<F, S, B, X, U> {
    config: Rc<DispatcherConfig<S, X, U>>,
    _t: marker::PhantomData<(F, B)>,
}

impl<F, S, B, X, U> Service<Io<F>> for H1ServiceHandler<F, S, B, X, U>
where
    F: Filter + 'static,
    S: Service<Request>,
    S::Error: ResponseError + 'static,
    S::Response: Into<Response<B>>,
    B: MessageBody,
    X: Service<Request, Response = Request>,
    X::Error: ResponseError + 'static,
    U: Service<(Request, Io<F>, Codec), Response = ()> + 'static,
    U::Error: fmt::Display + Error,
{
    type Response = ();
    type Error = DispatchError;
    type Future = Dispatcher<F, S, B, X, U>;

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

    fn call(&self, io: Io<F>) -> Self::Future {
        log::trace!(
            "New http1 connection, peer address {:?}",
            io.query::<types::PeerAddr>().get()
        );

        Dispatcher::new(io, self.config.clone())
    }
}
