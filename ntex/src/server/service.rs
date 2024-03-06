use std::{net::SocketAddr, rc::Rc, task::Context, task::Poll};

use log::error;

use crate::service::{boxed, Service, ServiceCtx, ServiceFactory};
use crate::util::{BoxFuture, Pool, PoolId, PoolRef};
use crate::{io::Io, time::Millis};

use super::{counter::CounterGuard, socket::Stream, Config, Token};

/// Server message
pub enum ServerMessage<T> {
    /// New content received
    New(T),
    /// Graceful shutdown in millis
    Shutdown(Millis),
    /// Force shutdown
    ForceShutdown,
}

pub(super) trait StreamServiceFactory: Send + Clone + 'static {
    type Factory: ServiceFactory<Io>;

    fn create(&self, _: Config) -> Self::Factory;
}

pub trait InternalServiceFactory<T>: Send {
    fn name(&self, token: Token) -> &str;

    fn set_tag(&mut self, token: Token, tag: &'static str);

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory<T>>;

    fn create(&self)
        -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService<T>)>, ()>>;
}

pub type BoxedServerService<T> =
    boxed::BoxService<(Option<CounterGuard>, ServerMessage<T>), (), ()>;

#[derive(Clone)]
pub(super) struct StreamService<T> {
    service: Rc<T>,
    tag: &'static str,
    pool: Pool,
    pool_ref: PoolRef,
}

impl<T> StreamService<T> {
    pub(crate) fn new(service: T, tag: &'static str, pid: PoolId) -> Self {
        StreamService {
            tag,
            pool: pid.pool(),
            pool_ref: pid.pool_ref(),
            service: Rc::new(service),
        }
    }
}

impl<T> Service<(Option<CounterGuard>, ServerMessage<Stream>)> for StreamService<T>
where
    T: Service<Io>,
{
    type Response = ();
    type Error = ();

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

    async fn call(
        &self,
        (guard, req): (Option<CounterGuard>, ServerMessage<Stream>),
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), ()> {
        match req {
            ServerMessage::New(stream) => {
                let stream = stream.try_into().map_err(|e| {
                    error!("Cannot convert to an async io stream: {}", e);
                });

                if let Ok(stream) = stream {
                    let stream: Io<_> = stream;
                    stream.set_tag(self.tag);
                    stream.set_memory_pool(self.pool_ref);
                    let _ = ctx.call(self.service.as_ref(), stream).await;
                    drop(guard);
                    Ok(())
                } else {
                    Err(())
                }
            }
            _ => Ok(()),
        }
    }
}

pub(super) struct Factory<F: StreamServiceFactory> {
    name: String,
    tag: &'static str,
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
        tag: &'static str,
    ) -> Box<dyn InternalServiceFactory<Stream>> {
        Box::new(Self {
            name,
            token,
            inner,
            addr,
            tag,
        })
    }
}

impl<F> InternalServiceFactory<Stream> for Factory<F>
where
    F: StreamServiceFactory,
{
    fn name(&self, _: Token) -> &str {
        &self.name
    }

    fn set_tag(&mut self, _: Token, tag: &'static str) {
        self.tag = tag;
    }

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory<Stream>> {
        Box::new(Self {
            name: self.name.clone(),
            inner: self.inner.clone(),
            token: self.token,
            addr: self.addr,
            tag: self.tag,
        })
    }

    fn create(
        &self,
    ) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService<Stream>)>, ()>> {
        let token = self.token;
        let tag = self.tag;
        let cfg = Config::default();
        let pool = cfg.get_pool_id();
        let factory = self.inner.create(cfg);

        Box::pin(async move {
            match factory.create(()).await {
                Ok(inner) => {
                    let service = boxed::service(StreamService::new(inner, tag, pool));
                    Ok(vec![(token, service)])
                }
                Err(_) => Err(()),
            }
        })
    }
}

impl<T> InternalServiceFactory<T> for Box<dyn InternalServiceFactory<T>> {
    fn name(&self, token: Token) -> &str {
        self.as_ref().name(token)
    }

    fn set_tag(&mut self, token: Token, tag: &'static str) {
        self.as_mut().set_tag(token, tag);
    }

    fn clone_factory(&self) -> Box<dyn InternalServiceFactory<T>> {
        self.as_ref().clone_factory()
    }

    fn create(
        &self,
    ) -> BoxFuture<'static, Result<Vec<(Token, BoxedServerService<T>)>, ()>> {
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
