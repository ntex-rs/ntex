use std::convert::TryInto;
use std::{
    future::Future, marker::PhantomData, net::SocketAddr, pin::Pin, task::Context,
    task::Poll,
};

use log::error;

use crate::io::Io;
use crate::service::{Service, ServiceFactory};
use crate::util::{counter::CounterGuard, Ready};
use crate::{rt::spawn, time::Millis};

use super::{socket::Stream, Token};

/// Server message
pub(super) enum ServerMessage {
    /// New stream
    Connect(Stream),
    /// Gracefull shutdown in millis
    Shutdown(Millis),
    /// Force shutdown
    ForceShutdown,
}

pub trait StreamServiceFactory: Send + Clone + 'static {
    type Factory: ServiceFactory<Config = (), Request = Io>;

    fn create(&self) -> Self::Factory;
}

pub(super) trait InternalServiceFactory: Send {
    fn name(&self, token: Token) -> &str;

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory>;

    fn create(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(Token, BoxedServerService)>, ()>>>>;
}

pub(super) type BoxedServerService = Box<
    dyn Service<
        Request = (Option<CounterGuard>, ServerMessage),
        Response = (),
        Error = (),
        Future = Ready<(), ()>,
    >,
>;

pub(super) struct StreamService<T> {
    service: T,
}

impl<T> StreamService<T> {
    pub(crate) fn new(service: T) -> Self {
        StreamService { service }
    }
}

impl<T> Service for StreamService<T>
where
    T: Service<Request = Io>,
    T::Future: 'static,
    T::Error: 'static,
{
    type Request = (Option<CounterGuard>, ServerMessage);
    type Response = ();
    type Error = ();
    type Future = Ready<(), ()>;

    #[inline]
    fn poll_ready(&self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(ctx).map_err(|_| ())
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, (guard, req): (Option<CounterGuard>, ServerMessage)) -> Self::Future {
        match req {
            ServerMessage::Connect(stream) => {
                let stream = stream.try_into().map_err(|e| {
                    error!("Cannot convert to an async io stream: {}", e);
                });

                if let Ok(stream) = stream {
                    let f = self.service.call(stream);
                    spawn(async move {
                        let _ = f.await;
                        drop(guard);
                    });
                    Ready::Ok(())
                } else {
                    Ready::Err(())
                }
            }
            _ => Ready::Ok(()),
        }
    }
}

pub(super) struct Factory<F: StreamServiceFactory> {
    name: String,
    inner: F,
    token: Token,
    addr: SocketAddr,
}

impl<F> Factory<F>
where
    F: StreamServiceFactory,
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
        })
    }
}

impl<F> InternalServiceFactory for Factory<F>
where
    F: StreamServiceFactory,
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
        })
    }

    fn create(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(Token, BoxedServerService)>, ()>>>>
    {
        let token = self.token;
        let fut = self.inner.create().new_service(());

        Box::pin(async move {
            match fut.await {
                Ok(inner) => {
                    let service: BoxedServerService =
                        Box::new(StreamService::new(inner));
                    Ok(vec![(token, service)])
                }
                Err(_) => Err(()),
            }
        })
    }
}

impl InternalServiceFactory for Box<dyn InternalServiceFactory> {
    fn name(&self, token: Token) -> &str {
        self.as_ref().name(token)
    }

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory> {
        self.as_ref().clone_factory()
    }

    fn create(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<(Token, BoxedServerService)>, ()>>>>
    {
        self.as_ref().create()
    }
}

impl<F, T> StreamServiceFactory for F
where
    F: Fn() -> T + Send + Clone + 'static,
    T: ServiceFactory<Config = (), Request = Io>,
{
    type Factory = T;

    #[inline]
    fn create(&self) -> T {
        (self)()
    }
}
