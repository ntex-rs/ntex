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
use crate::time::Seconds;

/// A http service builder
///
/// This type can be used to construct an instance of `http service` through a
/// builder-like pattern.
pub struct HttpServiceBuilder<F, S, X = ExpectHandler, U = UpgradeHandler<F>> {
    config: ServiceConfig,
    expect: X,
    upgrade: Option<U>,
    on_request: Option<OnRequest>,
    _t: PhantomData<(F, S)>,
}

impl<F, S> HttpServiceBuilder<F, S, ExpectHandler, UpgradeHandler<F>> {
    /// Create instance of `ServiceConfigBuilder`
    pub fn new() -> Self {
        HttpServiceBuilder::with_config(ServiceConfig::default())
    }

    #[doc(hidden)]
    /// Create instance of `ServiceConfigBuilder`
    pub fn with_config(config: ServiceConfig) -> Self {
        HttpServiceBuilder {
            config,
            expect: ExpectHandler,
            upgrade: None,
            on_request: None,
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
        self.config.keepalive(val);
        self
    }

    /// Set request headers read timeout.
    ///
    /// Defines a timeout for reading client request header. If a client does not transmit
    /// the entire set headers within this time, the request is terminated with
    /// the 408 (Request Time-out) error.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 3 seconds.
    pub fn client_timeout(mut self, timeout: Seconds) -> Self {
        self.config.client_timeout(timeout);
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
        self.config.disconnect_timeout(timeout);
        self
    }

    /// Set server ssl handshake timeout.
    ///
    /// Defines a timeout for connection ssl handshake negotiation.
    /// To disable timeout set value to 0.
    ///
    /// By default handshake timeout is set to 5 seconds.
    pub fn ssl_handshake_timeout(mut self, timeout: Seconds) -> Self {
        self.config.ssl_handshake_timeout(timeout);
        self
    }

    /// Set read rate parameters for request headers.
    ///
    /// Set max timeout for reading request headers. If the client
    /// sends `rate` amount of data, increase the timeout by 1 second for every.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default headers read rate is set to 1sec with max timeout 5sec.
    pub fn headers_read_rate(
        mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u16,
    ) -> Self {
        self.config.headers_read_rate(timeout, max_timeout, rate);
        self
    }

    /// Set read rate parameters for request's payload.
    ///
    /// Set max timeout for reading payload. If the client
    /// sends `rate` amount of data, increase the timeout by 1 second for every.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default payload read rate is disabled.
    pub fn payload_read_rate(
        mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u16,
    ) -> Self {
        self.config.payload_read_rate(timeout, max_timeout, rate);
        self
    }

    #[doc(hidden)]
    /// Configure http2 connection settings
    pub fn configure_http2<O, R>(self, f: O) -> Self
    where
        O: FnOnce(&h2::Config) -> R,
    {
        let _ = f(&self.config.h2config);
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
            config: self.config,
            expect: expect.into_factory(),
            upgrade: self.upgrade,
            on_request: self.on_request,
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
            config: self.config,
            expect: self.expect,
            upgrade: Some(upgrade.into_factory()),
            on_request: self.on_request,
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
        H1Service::with_config(self.config, service.into_factory())
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
        H2Service::with_config(self.config, service.into_factory())
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
        HttpService::with_config(self.config, service.into_factory())
            .expect(self.expect)
            .upgrade(self.upgrade)
            .on_request(self.on_request)
    }
}
