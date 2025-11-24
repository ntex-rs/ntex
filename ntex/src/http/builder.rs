use std::{error::Error, fmt, marker::PhantomData};

use crate::http::body::MessageBody;
use crate::http::config::{KeepAlive, ServiceConfig};
use crate::http::error::{H2Error, ResponseError};
use crate::http::h1::{self, H1Service};
use crate::http::h2::{self, H2Service};
use crate::http::{request::Request, response::Response, service::HttpService};
use crate::service::{IntoServiceFactory, ServiceFactory};
use crate::{io::Filter, io::SharedConfig, time::Seconds};

/// A http service builder
///
/// This type can be used to construct an instance of `http service` through a
/// builder-like pattern.
pub struct HttpServiceBuilder<
    F,
    S,
    C1 = h1::DefaultControlService,
    C2 = h2::DefaultControlService,
> {
    config: ServiceConfig,
    h1_control: C1,
    h2_control: C2,
    _t: PhantomData<(F, S)>,
}

impl<F, S> HttpServiceBuilder<F, S, h1::DefaultControlService, h2::DefaultControlService> {
    /// Create instance of `ServiceConfigBuilder`
    pub fn new() -> Self {
        HttpServiceBuilder::with_config(ServiceConfig::default())
    }

    #[doc(hidden)]
    /// Create instance of `ServiceConfigBuilder`
    pub fn with_config(config: ServiceConfig) -> Self {
        HttpServiceBuilder {
            config,
            h1_control: h1::DefaultControlService,
            h2_control: h2::DefaultControlService,
            _t: PhantomData,
        }
    }
}

impl<F, S, C1, C2> HttpServiceBuilder<F, S, C1, C2>
where
    F: Filter,
    S: ServiceFactory<Request, SharedConfig> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    C1: ServiceFactory<h1::Control<F, S::Error>, SharedConfig, Response = h1::ControlAck>,
    C1::Error: Error,
    C1::InitError: fmt::Debug,
    C2: ServiceFactory<h2::Control<H2Error>, SharedConfig, Response = h2::ControlAck>,
    C2::Error: Error,
    C2::InitError: fmt::Debug,
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
        rate: u32,
    ) -> Self {
        self.config.headers_read_rate(timeout, max_timeout, rate);
        self
    }

    /// Set read rate parameters for request's payload.
    ///
    /// Set read timeout, max timeout and rate for reading payload. If the client
    /// sends `rate` amount of data within `timeout` period of time, extend timeout by `timeout` seconds.
    /// But no more than `max_timeout` timeout.
    ///
    /// By default payload read rate is disabled.
    pub fn payload_read_rate(
        mut self,
        timeout: Seconds,
        max_timeout: Seconds,
        rate: u32,
    ) -> Self {
        self.config.payload_read_rate(timeout, max_timeout, rate);
        self
    }

    /// Provide control service for http/1.
    pub fn h1_control<CF, CT>(self, control: CF) -> HttpServiceBuilder<F, S, CT, C2>
    where
        CF: IntoServiceFactory<CT, h1::Control<F, S::Error>, SharedConfig>,
        CT: ServiceFactory<
                h1::Control<F, S::Error>,
                SharedConfig,
                Response = h1::ControlAck,
            >,
        CT::Error: Error,
        CT::InitError: fmt::Debug,
    {
        HttpServiceBuilder {
            config: self.config,
            h2_control: self.h2_control,
            h1_control: control.into_factory(),
            _t: PhantomData,
        }
    }

    /// Finish service configuration and create *http service* for HTTP/1 protocol.
    pub fn h1<B, SF>(self, service: SF) -> H1Service<F, S, B, C1>
    where
        B: MessageBody,
        SF: IntoServiceFactory<S, Request, SharedConfig>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
    {
        H1Service::with_config(self.config, service.into_factory()).control(self.h1_control)
    }

    /// Provide control service for http/2 protocol.
    pub fn h2_control<CF, CT>(self, control: CF) -> HttpServiceBuilder<F, S, C1, CT>
    where
        CF: IntoServiceFactory<CT, h2::Control<H2Error>, SharedConfig>,
        CT: ServiceFactory<h2::Control<H2Error>, SharedConfig, Response = h2::ControlAck>,
        CT::Error: Error,
        CT::InitError: fmt::Debug,
    {
        HttpServiceBuilder {
            config: self.config,
            h1_control: self.h1_control,
            h2_control: control.into_factory(),
            _t: PhantomData,
        }
    }

    /// Configure http2 connection settings
    pub fn h2_configure<O, R>(self, f: O) -> Self
    where
        O: FnOnce(&h2::Config) -> R,
    {
        let _ = f(&self.config.h2config);
        self
    }

    /// Finish service configuration and create *http service* for HTTP/2 protocol.
    pub fn h2<B, SF>(self, service: SF) -> H2Service<F, S, B, C2>
    where
        B: MessageBody + 'static,
        SF: IntoServiceFactory<S, Request, SharedConfig>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
    {
        H2Service::with_config(self.config, service.into_factory()).control(self.h2_control)
    }

    /// Finish service configuration and create `HttpService` instance.
    pub fn finish<B, SF>(self, service: SF) -> HttpService<F, S, B, C1, C2>
    where
        B: MessageBody + 'static,
        SF: IntoServiceFactory<S, Request, SharedConfig>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
    {
        HttpService::with_config(self.config, service.into_factory())
            .h1_control(self.h1_control)
            .h2_control(self.h2_control)
    }
}
