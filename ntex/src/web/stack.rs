use std::marker::PhantomData;

use crate::service::{Middleware, Service, ServiceCtx};
use crate::web::{ErrorRenderer, WebRequest, WebResponse};

/// Stack of middlewares.
#[derive(Debug, Clone)]
pub struct WebStack<Inner, Outer, Err> {
    inner: Inner,
    outer: Outer,
    err: PhantomData<Err>,
}

impl<Inner, Outer, Err> WebStack<Inner, Outer, Err> {
    pub fn new(inner: Inner, outer: Outer) -> Self {
        WebStack {
            inner,
            outer,
            err: PhantomData,
        }
    }
}

impl<S, Inner, Outer, Err> Middleware<S> for WebStack<Inner, Outer, Err>
where
    Inner: Middleware<S>,
    Outer: Middleware<Inner::Service>,
    Outer::Service: Service<WebRequest<Err>, Response = WebResponse>,
{
    type Service = WebMiddleware<Outer::Service, Err>;

    fn create(&self, service: S) -> Self::Service {
        WebMiddleware {
            svc: self.outer.create(self.inner.create(service)),
            err: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct WebMiddleware<S, Err> {
    svc: S,
    err: PhantomData<Err>,
}

impl<S, Err> Clone for WebMiddleware<S, Err>
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

impl<S, Err> Service<WebRequest<Err>> for WebMiddleware<S, Err>
where
    S: Service<WebRequest<Err>, Response = WebResponse>,
    Err: ErrorRenderer,
    Err::Container: From<S::Error>,
{
    type Response = WebResponse;
    type Error = Err::Container;

    #[inline]
    async fn call(
        &self,
        req: WebRequest<Err>,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.svc, req).await.map_err(Into::into)
    }

    crate::forward_poll!(svc);
    crate::forward_ready!(svc);
    crate::forward_shutdown!(svc);
}
