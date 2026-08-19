use std::marker::PhantomData;

use crate::service::{Ctx, Middleware, Service, cfg::SharedCfg};
use crate::web::{ErrorRenderer, WebRequest, WebResponse};

/// Stack of middlewares.
#[derive(Debug, Clone)]
pub struct WebStack<St, Inner, Outer, Err> {
    inner: Inner,
    outer: Outer,
    err: PhantomData<(St, Err)>,
}

impl<St, Inner, Outer, Err> WebStack<St, Inner, Outer, Err> {
    pub fn new(inner: Inner, outer: Outer) -> Self {
        WebStack {
            inner,
            outer,
            err: PhantomData,
        }
    }
}

impl<S, St, Inner, Outer, Err> Middleware<S, SharedCfg> for WebStack<St, Inner, Outer, Err>
where
    Inner: Middleware<S, SharedCfg>,
    Outer: Middleware<Inner::Service, SharedCfg>,
    Outer::Service: Service<St, Req = WebRequest<Err>, Res = WebResponse>,
{
    type Service = WebMiddleware<Outer::Service, St, Err>;

    fn create(&self, service: S, cfg: &SharedCfg) -> Self::Service {
        WebMiddleware {
            svc: self.outer.create(self.inner.create(service, cfg), cfg),
            err: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct WebMiddleware<S, St, Err> {
    svc: S,
    err: PhantomData<(St, Err)>,
}

impl<S, St, Err> Clone for WebMiddleware<S, St, Err>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self {
            svc: self.svc.clone(),
            err: PhantomData,
        }
    }
}

impl<S, St, Err> Service<St> for WebMiddleware<S, St, Err>
where
    S: Service<St, Req = WebRequest<Err>, Res = WebResponse>,
    Err: ErrorRenderer,
    Err::Container: From<S::Error>,
{
    type Req = WebRequest<Err>;
    type Res = WebResponse;
    type Error = Err::Container;

    #[inline]
    async fn call(
        &self,
        req: WebRequest<Err>,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        ctx.call(&self.svc, req).await.map_err(Into::into)
    }

    crate::forward_ready!(St, svc);
    crate::forward_shutdown!(svc);
}
