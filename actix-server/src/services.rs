use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use actix_rt::spawn;
use actix_server_config::{Io, ServerConfig};
use actix_service::{NewService, Service};
use futures::future::{err, ok, FutureResult};
use futures::{Future, Poll};
use log::error;

use super::Token;
use crate::counter::CounterGuard;
use crate::socket::{FromStream, StdStream};

/// Server message
pub(crate) enum ServerMessage {
    /// New stream
    Connect(StdStream),
    /// Gracefull shutdown
    Shutdown(Duration),
    /// Force shutdown
    ForceShutdown,
}

pub trait ServiceFactory<Stream: FromStream>: Send + Clone + 'static {
    type NewService: NewService<Config = ServerConfig, Request = Io<Stream>>;

    fn create(&self) -> Self::NewService;
}

pub(crate) trait InternalServiceFactory: Send {
    fn name(&self, token: Token) -> &str;

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory>;

    fn create(&self) -> Box<dyn Future<Item = Vec<(Token, BoxedServerService)>, Error = ()>>;
}

pub(crate) type BoxedServerService = Box<
    dyn Service<
        Request = (Option<CounterGuard>, ServerMessage),
        Response = (),
        Error = (),
        Future = FutureResult<(), ()>,
    >,
>;

pub(crate) struct StreamService<T> {
    service: T,
}

impl<T> StreamService<T> {
    pub(crate) fn new(service: T) -> Self {
        StreamService { service }
    }
}

impl<T, I> Service for StreamService<T>
where
    T: Service<Request = Io<I>>,
    T::Future: 'static,
    T::Error: 'static,
    I: FromStream,
{
    type Request = (Option<CounterGuard>, ServerMessage);
    type Response = ();
    type Error = ();
    type Future = FutureResult<(), ()>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(|_| ())
    }

    fn call(&mut self, (guard, req): (Option<CounterGuard>, ServerMessage)) -> Self::Future {
        match req {
            ServerMessage::Connect(stream) => {
                let stream = FromStream::from_stdstream(stream).map_err(|e| {
                    error!("Can not convert to an async tcp stream: {}", e);
                });

                if let Ok(stream) = stream {
                    spawn(self.service.call(Io::new(stream)).then(move |res| {
                        drop(guard);
                        res.map_err(|_| ()).map(|_| ())
                    }));
                    ok(())
                } else {
                    err(())
                }
            }
            _ => ok(()),
        }
    }
}

pub(crate) struct StreamNewService<F: ServiceFactory<Io>, Io: FromStream> {
    name: String,
    inner: F,
    token: Token,
    addr: SocketAddr,
    _t: PhantomData<Io>,
}

impl<F, Io> StreamNewService<F, Io>
where
    F: ServiceFactory<Io>,
    Io: FromStream + Send + 'static,
{
    pub(crate) fn create(
        name: String,
        token: Token,
        inner: F,
        addr: SocketAddr,
    ) -> Box<dyn InternalServiceFactory> {
        Box::new(Self {
            name,
            token,
            inner,
            addr,
            _t: PhantomData,
        })
    }
}

impl<F, Io> InternalServiceFactory for StreamNewService<F, Io>
where
    F: ServiceFactory<Io>,
    Io: FromStream + Send + 'static,
{
    fn name(&self, _: Token) -> &str {
        &self.name
    }

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory> {
        Box::new(Self {
            name: self.name.clone(),
            inner: self.inner.clone(),
            token: self.token,
            addr: self.addr,
            _t: PhantomData,
        })
    }

    fn create(&self) -> Box<dyn Future<Item = Vec<(Token, BoxedServerService)>, Error = ()>> {
        let token = self.token;
        let config = ServerConfig::new(self.addr);
        Box::new(
            self.inner
                .create()
                .new_service(&config)
                .map_err(|_| ())
                .map(move |inner| {
                    let service: BoxedServerService = Box::new(StreamService::new(inner));
                    vec![(token, service)]
                }),
        )
    }
}

impl InternalServiceFactory for Box<dyn InternalServiceFactory> {
    fn name(&self, token: Token) -> &str {
        self.as_ref().name(token)
    }

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory> {
        self.as_ref().clone_factory()
    }

    fn create(&self) -> Box<dyn Future<Item = Vec<(Token, BoxedServerService)>, Error = ()>> {
        self.as_ref().create()
    }
}

impl<F, T, I> ServiceFactory<I> for F
where
    F: Fn() -> T + Send + Clone + 'static,
    T: NewService<Config = ServerConfig, Request = Io<I>>,
    I: FromStream,
{
    type NewService = T;

    fn create(&self) -> T {
        (self)()
    }
}
