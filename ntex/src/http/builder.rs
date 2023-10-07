use std::{error::Error, fmt, marker::PhantomData};

use ntex_h2::{self as h2};

use crate::http::body::MessageBody;
use crate::http::config::{KeepAlive, OnRequest, ServiceConfig};
use crate::http::error::ResponseError;
use crate::http::h1::{Codec, ExpectHandler, H1Service, UpgradeHandler};
use crate::http::h2::H2Service;
use crate::http::request::Request;
use crate::http::response::Response;
use crate::http::service::HttpService;
use crate::io::{Filter, Io, IoRef};
use crate::service::{boxed, IntoService, IntoServiceFactory, Service, ServiceFactory};
use crate::time::{Millis, Seconds};

/// A http service builder
///
/// This type can be used to construct an instance of `http service` through a
/// builder-like pattern.
#[derive(Debug)]
pub struct HttpServiceBuilder<F, S, X = ExpectHandler, U = UpgradeHandler<F>> {
    keep_alive: KeepAlive,
    client_timeout: Millis,
    client_disconnect: Seconds,
    handshake_timeout: Millis,
    expect: X,
    upgrade: Option<U>,
    on_request: Option<OnRequest>,
    h2config: h2::Config,
    _t: PhantomData<(F, S)>,
}

impl<F, S> HttpServiceBuilder<F, S, ExpectHandler, UpgradeHandler<F>> {
    /// Create instance of `ServiceConfigBuilder`
    pub fn new() -> Self {
        HttpServiceBuilder {
            keep_alive: KeepAlive::Timeout(Seconds(5)),
            client_timeout: Millis::from_secs(3),
            client_disconnect: Seconds(3),
            handshake_timeout: Millis::from_secs(5),
            expect: ExpectHandler,
            upgrade: None,
            on_request: None,
            h2config: h2::Config::server(),
            _t: PhantomData,
        }
    }
}

impl<F, S, X, U> HttpServiceBuilder<F, S, X, U>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    X: ServiceFactory<Request, Response = Request> + 'static,
    X::Error: ResponseError,
    X::InitError: fmt::Debug,
    U: ServiceFactory<(Request, Io<F>, Codec), Response = ()> + 'static,
    U::Error: fmt::Display + Error,
    U::InitError: fmt::Debug,
{
    /// Set server keep-alive setting.
    ///
    /// By default keep alive is set to a 5 seconds.
    pub fn keep_alive<W: Into<KeepAlive>>(mut self, val: W) -> Self {
        self.keep_alive = val.into();
        self
    }

    /// Set server client timeout for first request.
    ///
    /// Defines a timeout for reading client request header. If a client does not transmit
    /// the entire set headers within this time, the request is terminated with
    /// the 408 (Request Time-out) error.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 3 seconds.
    pub fn client_timeout(mut self, timeout: Seconds) -> Self {
        self.client_timeout = timeout.into();
        self.h2config.client_timeout(timeout);
        self
    }

    /// Set server connection disconnect timeout in seconds.
    ///
    /// Defines a timeout for disconnect connection. If a disconnect procedure does not complete
    /// within this time, the connection get dropped.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default disconnect timeout is set to 3 seconds.
    pub fn disconnect_timeout(mut self, timeout: Seconds) -> Self {
        self.client_disconnect = timeout;
        self.h2config.disconnect_timeout(timeout);
        self
    }

    /// Set server ssl handshake timeout.
    ///
    /// Defines a timeout for connection ssl handshake negotiation.
    /// To disable timeout set value to 0.
    ///
    /// By default handshake timeout is set to 5 seconds.
    pub fn ssl_handshake_timeout(mut self, timeout: Seconds) -> Self {
        self.handshake_timeout = timeout.into();
        self.h2config.handshake_timeout(timeout);
        self
    }

    #[doc(hidden)]
    /// Configure http2 connection settings
    pub fn configure_http2<O, R>(self, f: O) -> Self
    where
        O: FnOnce(&h2::Config) -> R,
    {
        let _ = f(&self.h2config);
        self
    }

    /// Provide service for `EXPECT: 100-Continue` support.
    ///
    /// Service get called with request that contains `EXPECT` header.
    /// Service must return request in case of success, in that case
    /// request will be forwarded to main service.
    pub fn expect<XF, X1>(self, expect: XF) -> HttpServiceBuilder<F, S, X1, U>
    where
        XF: IntoServiceFactory<X1, Request>,
        X1: ServiceFactory<Request, Response = Request>,
        X1::InitError: fmt::Debug,
    {
        HttpServiceBuilder {
            keep_alive: self.keep_alive,
            client_timeout: self.client_timeout,
            client_disconnect: self.client_disconnect,
            handshake_timeout: self.handshake_timeout,
            expect: expect.into_factory(),
            upgrade: self.upgrade,
            on_request: self.on_request,
            h2config: self.h2config,
            _t: PhantomData,
        }
    }

    /// Provide service for custom `Connection: UPGRADE` support.
    ///
    /// If service is provided then normal requests handling get halted
    /// and this service get called with original request and framed object.
    pub fn upgrade<UF, U1>(self, upgrade: UF) -> HttpServiceBuilder<F, S, X, U1>
    where
        UF: IntoServiceFactory<U1, (Request, Io<F>, Codec)>,
        U1: ServiceFactory<(Request, Io<F>, Codec), Response = ()>,
        U1::Error: fmt::Display + Error,
        U1::InitError: fmt::Debug,
    {
        HttpServiceBuilder {
            keep_alive: self.keep_alive,
            client_timeout: self.client_timeout,
            client_disconnect: self.client_disconnect,
            handshake_timeout: self.handshake_timeout,
            expect: self.expect,
            upgrade: Some(upgrade.into_factory()),
            on_request: self.on_request,
            h2config: self.h2config,
            _t: PhantomData,
        }
    }

    /// Set req request callback.
    ///
    /// It get called once per request.
    pub fn on_request<R, FR>(mut self, f: FR) -> Self
    where
        FR: IntoService<R, (Request, IoRef)>,
        R: Service<(Request, IoRef), Response = Request, Error = Response> + 'static,
    {
        self.on_request = Some(boxed::service(f.into_service()));
        self
    }

    /// Finish service configuration and create *http service* for HTTP/1 protocol.
    pub fn h1<B, SF>(self, service: SF) -> H1Service<F, S, B, X, U>
    where
        B: MessageBody,
        SF: IntoServiceFactory<S, Request>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
    {
        let cfg = ServiceConfig::new(
            self.keep_alive,
            self.client_timeout,
            self.client_disconnect,
            self.handshake_timeout,
            self.h2config,
        );
        H1Service::with_config(cfg, service.into_factory())
            .expect(self.expect)
            .upgrade(self.upgrade)
            .on_request(self.on_request)
    }

    /// Finish service configuration and create *http service* for HTTP/2 protocol.
    pub fn h2<B, SF>(self, service: SF) -> H2Service<F, S, B>
    where
        B: MessageBody + 'static,
        SF: IntoServiceFactory<S, Request>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
    {
        let cfg = ServiceConfig::new(
            self.keep_alive,
            self.client_timeout,
            self.client_disconnect,
            self.handshake_timeout,
            self.h2config,
        );

        H2Service::with_config(cfg, service.into_factory())
    }

    /// Finish service configuration and create `HttpService` instance.
    pub fn finish<B, SF>(self, service: SF) -> HttpService<F, S, B, X, U>
    where
        B: MessageBody + 'static,
        SF: IntoServiceFactory<S, Request>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
    {
        let cfg = ServiceConfig::new(
            self.keep_alive,
            self.client_timeout,
            self.client_disconnect,
            self.handshake_timeout,
            self.h2config,
        );
        HttpService::with_config(cfg, service.into_factory())
            .expect(self.expect)
            .upgrade(self.upgrade)
            .on_request(self.on_request)
    }
}
