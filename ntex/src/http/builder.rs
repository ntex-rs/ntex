use std::{error::Error, fmt, marker::PhantomData};

use ntex_h2::{self as h2};

use crate::http::body::MessageBody;
use crate::http::config::{KeepAlive, ServiceConfig};
use crate::http::error::ResponseError;
use crate::http::h1::{self, DefaultControlService, H1Service};
use crate::http::h2::H2Service;
use crate::http::{request::Request, response::Response, service::HttpService};
use crate::service::{IntoServiceFactory, ServiceFactory};
use crate::{io::Filter, time::Seconds};

/// A http service builder
///
/// This type can be used to construct an instance of `http service` through a
/// builder-like pattern.
pub struct HttpServiceBuilder<F, S, C = DefaultControlService> {
    config: ServiceConfig,
    control: C,
    _t: PhantomData<(F, S)>,
}

impl<F, S> HttpServiceBuilder<F, S, DefaultControlService> {
    /// Create instance of `ServiceConfigBuilder`
    pub fn new() -> Self {
        HttpServiceBuilder::with_config(ServiceConfig::default())
    }

    #[doc(hidden)]
    /// Create instance of `ServiceConfigBuilder`
    pub fn with_config(config: ServiceConfig) -> Self {
        HttpServiceBuilder {
            config,
            control: DefaultControlService,
            _t: PhantomData,
        }
    }
}

impl<F, S, C> HttpServiceBuilder<F, S, C>
where
    F: Filter,
    S: ServiceFactory<Request> + 'static,
    S::Error: ResponseError,
    S::InitError: fmt::Debug,
    C: ServiceFactory<h1::Control<F, S::Error>, Response = h1::ControlAck>,
    C::Error: Error,
    C::InitError: Error,
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
    /// Set read timeout, max timeout and rate for reading payload. If the client
    /// sends `rate` amount of data within `timeout` period of time, extend timeout by `timeout` seconds.
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

    /// Provide control service.
    pub fn control<CF, C1>(self, control: CF) -> HttpServiceBuilder<F, S, C1>
    where
        CF: IntoServiceFactory<C1, h1::Control<F, S::Error>>,
        C1: ServiceFactory<h1::Control<F, S::Error>, Response = h1::ControlAck>,
        C1::Error: Error,
        C1::InitError: Error,
    {
        HttpServiceBuilder {
            config: self.config,
            control: control.into_factory(),
            _t: PhantomData,
        }
    }

    /// Finish service configuration and create *http service* for HTTP/1 protocol.
    pub fn h1<B, SF>(self, service: SF) -> H1Service<F, S, B, C>
    where
        B: MessageBody,
        SF: IntoServiceFactory<S, Request>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
    {
        H1Service::with_config(self.config, service.into_factory()).control(self.control)
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
    pub fn finish<B, SF>(self, service: SF) -> HttpService<F, S, B, C>
    where
        B: MessageBody + 'static,
        SF: IntoServiceFactory<S, Request>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
    {
        HttpService::with_config(self.config, service.into_factory()).control(self.control)
    }
}
