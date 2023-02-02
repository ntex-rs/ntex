use std::{net::SocketAddr, rc::Rc, task::Context, task::Poll};

use log::error;

use crate::io::Io;
use crate::service::{boxed, Service, ServiceFactory};
use crate::util::{BoxFuture, Pool, PoolId, Ready};
use crate::{rt::spawn, time::Millis};

use super::{counter::CounterGuard, socket::Stream, Config, Token};

/// Server message
pub(super) enum ServerMessage {
    /// New stream
    Connect(Stream),
    /// Gracefull shutdown in millis
    Shutdown(Millis),
    /// Force shutdown
    ForceShutdown,
}

pub(super) trait StreamServiceFactory: Send + Clone + 'static {
    type Factory: ServiceFactory<Io>;

    fn create(&self, _: Config) -> Self::Factory;
}

pub(super) trait InternalServiceFactory: Send {
    fn name(&self, token: Token) -> &str;

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory>;

    fn create(&self) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>>;
}

pub(super) type BoxedServerService =
    boxed::RcService<(Option<CounterGuard>, ServerMessage), (), ()>;

#[derive(Clone)]
pub(super) struct StreamService<T> {
    service: Rc<T>,
    pool: Pool,
}

impl<T> StreamService<T> {
    pub(crate) fn new(service: T, pid: PoolId) -> Self {
        StreamService {
            pool: pid.pool(),
            service: Rc::new(service),
        }
    }
}

impl<T> Service<(Option<CounterGuard>, ServerMessage)> for StreamService<T>
where
    T: Service<Io> + 'static,
    T::Error: 'static,
{
    type Response = ();
    type Error = ();
    type Future<'f> = Ready<(), ()> where T: 'f;

    crate::forward_poll_shutdown!(service);

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready = self.service.poll_ready(cx).map_err(|_| ())?.is_ready();
        let ready = self.pool.poll_ready(cx).is_ready() && ready;
        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(
        &self,
        (guard, req): (Option<CounterGuard>, ServerMessage),
    ) -> Self::Future<'_> {
        match req {
            ServerMessage::Connect(stream) => {
                let stream = stream.try_into().map_err(|e| {
                    error!("Cannot convert to an async io stream: {}", e);
                });

                if let Ok(stream) = stream {
                    let stream: Io<_> = stream;
                    stream.set_memory_pool(self.pool.pool_ref());
                    let svc = self.service.clone();
                    spawn(async move {
                        let _ = svc.call(stream).await;
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

    fn create(&self) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>> {
        let token = self.token;
        let cfg = Config::default();
        let pool = cfg.get_pool_id();
        let factory = self.inner.create(cfg);

        Box::pin(async move {
            match factory.create(()).await {
                Ok(inner) => {
                    let service: BoxedServerService =
                        boxed::rcservice(StreamService::new(inner, pool));
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

    fn create(&self) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService)>, ()>> {
        self.as_ref().create()
    }
}

impl<F, T> StreamServiceFactory for F
where
    F: Fn(Config) -> T + Send + Clone + 'static,
    T: ServiceFactory<Io>,
{
    type Factory = T;

    #[inline]
    fn create(&self, cfg: Config) -> T {
        (self)(cfg)
    }
}
