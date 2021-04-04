use std::{cell::RefCell, error::Error, fmt, marker::PhantomData, rc::Rc};

use crate::framed::State;
use crate::http::body::MessageBody;
use crate::http::config::{KeepAlive, OnRequest, ServiceConfig};
use crate::http::error::ResponseError;
use crate::http::h1::{Codec, ExpectHandler, H1Service, UpgradeHandler};
use crate::http::h2::H2Service;
use crate::http::helpers::{Data, DataFactory};
use crate::http::request::Request;
use crate::http::response::Response;
use crate::http::service::HttpService;
use crate::service::{boxed, IntoService, IntoServiceFactory, Service, ServiceFactory};

/// A http service builder
///
/// This type can be used to construct an instance of `http service` through a
/// builder-like pattern.
pub struct HttpServiceBuilder<T, S, X = ExpectHandler, U = UpgradeHandler<T>> {
    keep_alive: KeepAlive,
    client_timeout: u64,
    client_disconnect: u64,
    handshake_timeout: u64,
    lw: u16,
    read_hw: u16,
    write_hw: u16,
    expect: X,
    upgrade: Option<U>,
    on_connect: Option<Rc<dyn Fn(&T) -> Box<dyn DataFactory>>>,
    on_request: Option<OnRequest<T>>,
    _t: PhantomData<(T, S)>,
}

impl<T, S> HttpServiceBuilder<T, S, ExpectHandler, UpgradeHandler<T>> {
    /// Create instance of `ServiceConfigBuilder`
    pub fn new() -> Self {
        HttpServiceBuilder {
            keep_alive: KeepAlive::Timeout(5),
            client_timeout: 3,
            client_disconnect: 3,
            handshake_timeout: 5,
            lw: 1024,
            read_hw: 8 * 1024,
            write_hw: 8 * 1024,
            expect: ExpectHandler,
            upgrade: None,
            on_connect: None,
            on_request: None,
            _t: PhantomData,
        }
    }
}

impl<T, S, X, U> HttpServiceBuilder<T, S, X, U>
where
    S: ServiceFactory<Config = (), Request = Request>,
    S::Error: ResponseError + 'static,
    S::InitError: fmt::Debug,
    S::Future: 'static,
    <S::Service as Service>::Future: 'static,
    X: ServiceFactory<Config = (), Request = Request, Response = Request>,
    X::Error: ResponseError + 'static,
    X::InitError: fmt::Debug,
    X::Future: 'static,
    <X::Service as Service>::Future: 'static,
    U: ServiceFactory<Config = (), Request = (Request, T, State, Codec), Response = ()>,
    U::Error: fmt::Display + Error + 'static,
    U::InitError: fmt::Debug,
    U::Future: 'static,
    <U::Service as Service>::Future: 'static,
{
    /// Set server keep-alive setting.
    ///
    /// By default keep alive is set to a 5 seconds.
    pub fn keep_alive<W: Into<KeepAlive>>(mut self, val: W) -> Self {
        self.keep_alive = val.into();
        self
    }

    /// Set server client timeout in seconds for first request.
    ///
    /// Defines a timeout for reading client request header. If a client does not transmit
    /// the entire set headers within this time, the request is terminated with
    /// the 408 (Request Time-out) error.
    ///
    /// To disable timeout set value to 0.
    ///
    /// By default client timeout is set to 3 seconds.
    pub fn client_timeout(mut self, val: u16) -> Self {
        self.client_timeout = val as u64;
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
    pub fn disconnect_timeout(mut self, val: u16) -> Self {
        self.client_disconnect = val as u64;
        self
    }

    /// Set server ssl handshake timeout in seconds.
    ///
    /// Defines a timeout for connection ssl handshake negotiation.
    /// To disable timeout set value to 0.
    ///
    /// By default handshake timeout is set to 5 seconds.
    pub fn ssl_handshake_timeout(mut self, val: u16) -> Self {
        self.handshake_timeout = val as u64;
        self
    }

    #[inline]
    /// Set read/write buffer params
    ///
    /// By default read buffer is 8kb, write buffer is 8kb
    pub fn buffer_params(
        mut self,
        max_read_buf_size: u16,
        max_write_buf_size: u16,
        min_buf_size: u16,
    ) -> Self {
        self.read_hw = max_read_buf_size;
        self.write_hw = max_write_buf_size;
        self.lw = min_buf_size;
        self
    }

    /// Provide service for `EXPECT: 100-Continue` support.
    ///
    /// Service get called with request that contains `EXPECT` header.
    /// Service must return request in case of success, in that case
    /// request will be forwarded to main service.
    pub fn expect<F, X1>(self, expect: F) -> HttpServiceBuilder<T, S, X1, U>
    where
        F: IntoServiceFactory<X1>,
        X1: ServiceFactory<Config = (), Request = Request, Response = Request>,
        X1::Error: ResponseError + 'static,
        X1::InitError: fmt::Debug,
        X1::Future: 'static,
        <X1::Service as Service>::Future: 'static,
    {
        HttpServiceBuilder {
            keep_alive: self.keep_alive,
            client_timeout: self.client_timeout,
            client_disconnect: self.client_disconnect,
            handshake_timeout: self.handshake_timeout,
            expect: expect.into_factory(),
            upgrade: self.upgrade,
            on_connect: self.on_connect,
            on_request: self.on_request,
            lw: self.lw,
            read_hw: self.read_hw,
            write_hw: self.write_hw,
            _t: PhantomData,
        }
    }

    /// Provide service for custom `Connection: UPGRADE` support.
    ///
    /// If service is provided then normal requests handling get halted
    /// and this service get called with original request and framed object.
    pub fn upgrade<F, U1>(self, upgrade: F) -> HttpServiceBuilder<T, S, X, U1>
    where
        F: IntoServiceFactory<U1>,
        U1: ServiceFactory<
            Config = (),
            Request = (Request, T, State, Codec),
            Response = (),
        >,
        U1::Error: fmt::Display + Error + 'static,
        U1::InitError: fmt::Debug,
        U1::Future: 'static,
        <U1::Service as Service>::Future: 'static,
    {
        HttpServiceBuilder {
            keep_alive: self.keep_alive,
            client_timeout: self.client_timeout,
            client_disconnect: self.client_disconnect,
            handshake_timeout: self.handshake_timeout,
            expect: self.expect,
            upgrade: Some(upgrade.into_factory()),
            on_connect: self.on_connect,
            on_request: self.on_request,
            lw: self.lw,
            read_hw: self.read_hw,
            write_hw: self.write_hw,
            _t: PhantomData,
        }
    }

    /// Set on-connect callback.
    ///
    /// It get called once per connection and result of the call
    /// get stored to the request's extensions.
    pub fn on_connect<F, I>(mut self, f: F) -> Self
    where
        F: Fn(&T) -> I + 'static,
        I: Clone + 'static,
    {
        self.on_connect = Some(Rc::new(move |io| Box::new(Data(f(io)))));
        self
    }

    /// Set req request callback.
    ///
    /// It get called once per request.
    pub fn on_request<Filter, F>(mut self, f: F) -> Self
    where
        F: IntoService<Filter>,
        Filter: Service<
                Request = (Request, Rc<RefCell<T>>),
                Response = Request,
                Error = Response,
            > + 'static,
    {
        self.on_request = Some(boxed::service(f.into_service()));
        self
    }

    /// Finish service configuration and create *http service* for HTTP/1 protocol.
    pub fn h1<F, B>(self, service: F) -> H1Service<T, S, B, X, U>
    where
        B: MessageBody,
        F: IntoServiceFactory<S>,
        S::Error: ResponseError,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>>,
        S::Future: 'static,
    {
        let cfg = ServiceConfig::new(
            self.keep_alive,
            self.client_timeout,
            self.client_disconnect,
            self.handshake_timeout,
            self.lw,
            self.read_hw,
            self.write_hw,
        );
        H1Service::with_config(cfg, service.into_factory())
            .expect(self.expect)
            .upgrade(self.upgrade)
            .on_connect(self.on_connect)
            .on_request(self.on_request)
    }

    /// Finish service configuration and create *http service* for HTTP/2 protocol.
    pub fn h2<F, B>(self, service: F) -> H2Service<T, S, B>
    where
        B: MessageBody + 'static,
        F: IntoServiceFactory<S>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
        <S::Service as Service>::Future: 'static,
    {
        let cfg = ServiceConfig::new(
            self.keep_alive,
            self.client_timeout,
            self.client_disconnect,
            self.handshake_timeout,
            self.lw,
            self.read_hw,
            self.write_hw,
        );
        H2Service::with_config(cfg, service.into_factory()).on_connect(self.on_connect)
    }

    /// Finish service configuration and create `HttpService` instance.
    pub fn finish<F, B>(self, service: F) -> HttpService<T, S, B, X, U>
    where
        B: MessageBody + 'static,
        F: IntoServiceFactory<S>,
        S::Error: ResponseError + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
        S::Future: 'static,
        <S::Service as Service>::Future: 'static,
    {
        let cfg = ServiceConfig::new(
            self.keep_alive,
            self.client_timeout,
            self.client_disconnect,
            self.handshake_timeout,
            self.lw,
            self.read_hw,
            self.write_hw,
        );
        HttpService::with_config(cfg, service.into_factory())
            .expect(self.expect)
            .upgrade(self.upgrade)
            .on_connect(self.on_connect)
            .on_request(self.on_request)
    }
}
