use std::marker::PhantomData;

use crate::service::{Ctx, Middleware, Service, ServiceFactory};
use crate::web::{AppState, WebRequest, WebResponse};

/// Stack of middlewares.
#[derive(Debug, Clone)]
pub struct WebStack<St, Inner, Outer> {
    inner: Inner,
    outer: Outer,
    err: PhantomData<St>,
}

impl<St, Inner, Outer> WebStack<St, Inner, Outer> {
    pub fn new(inner: Inner, outer: Outer) -> Self {
        WebStack {
            inner,
            outer,
            err: PhantomData,
        }
    }
}

impl<S, St, Cfg, Inner, Outer> Middleware<S, St, Cfg> for WebStack<St, Inner, Outer>
where
    St: AppState,
    Inner: Middleware<S, St, Cfg>,
    Outer: Middleware<Inner::Service, St, Cfg>,
    Outer::Service: Service<St, WebRequest, Res = WebResponse>,
{
    type Service = WebMiddleware<Outer::Service, St>;

    fn create(&self, service: S, cfg: &Cfg) -> Self::Service {
        WebMiddleware {
            svc: self.outer.create(self.inner.create(service, cfg), cfg),
            err: PhantomData,
        }
    }
}

#[derive(Debug)]
pub struct WebMiddleware<S, St> {
    svc: S,
    err: PhantomData<St>,
}

impl<S, St> Clone for WebMiddleware<S, St>
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

impl<S, St> Service<St, WebRequest> for WebMiddleware<S, St>
where
    S: Service<St, WebRequest, Res = WebResponse>,
    St: AppState,
    St::Error: From<S::Error>,
{
    type Res = WebResponse;
    type Error = St::Error;

    #[inline]
    async fn call(
        &self,
        req: WebRequest,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        ctx.call(&self.svc, req).await.map_err(Into::into)
    }

    crate::forward_ready!(St, svc);
    crate::forward_shutdown!(St, svc);
}

#[derive(derive_more::Debug)]
#[debug("Filter")]
pub struct Filter<St>(PhantomData<St>);

impl<St> Filter<St> {
    pub(super) fn new() -> Self {
        Filter(PhantomData)
    }
}

impl<St: AppState, Cfg> ServiceFactory<St, WebRequest, Cfg> for Filter<St> {
    type Res = WebRequest;
    type Error = St::Error;

    type Service = Filter<St>;
    type InitError = ();

    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(Filter(PhantomData))
    }
}

impl<St: AppState> Service<St, WebRequest> for Filter<St> {
    type Res = WebRequest;
    type Error = St::Error;

    async fn call(&self, req: WebRequest, _: Ctx<'_, Self, St>) -> Result<WebRequest, St::Error> {
        Ok(req)
    }
}
