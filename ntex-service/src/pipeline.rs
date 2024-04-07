use std::future::{poll_fn, Future};
use std::{cell::Cell, fmt, pin::Pin, rc::Rc, task, task::Context, task::Poll};

use crate::{ctx::Waiters, Service, ServiceCtx};

#[derive(Debug)]
/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<S> {
    svc: Rc<S>,
    pending: Cell<bool>,
    pub(crate) waiters: Waiters,
}

impl<S> Pipeline<S> {
    #[inline]
    /// Construct new container instance.
    pub fn new(svc: S) -> Self {
        Pipeline {
            svc: Rc::new(svc),
            pending: Cell::new(false),
            waiters: Waiters::new(),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        self.svc.as_ref()
    }

    #[inline]
    /// Returns when the service is able to process requests.
    pub async fn ready<R>(&self) -> Result<(), S::Error>
    where
        S: Service<R>,
    {
        poll_fn(move |cx| self.poll_ready(cx)).await
    }

    #[inline]
    /// Returns `Ready` when the service is able to process requests.
    pub fn poll_ready<R>(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>>
    where
        S: Service<R>,
    {
        let res = self.svc.poll_ready(cx);
        if res.is_pending() {
            self.pending.set(true);
            self.waiters.register(cx)
        } else if self.pending.get() {
            self.pending.set(false);
            self.waiters.notify()
        }
        res
    }

    #[inline]
    /// Shutdown enclosed service.
    pub fn poll_shutdown<R>(&self, cx: &mut Context<'_>) -> Poll<()>
    where
        S: Service<R>,
    {
        self.svc.poll_shutdown(cx)
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub async fn call<R>(&self, req: R) -> Result<S::Response, S::Error>
    where
        S: Service<R>,
    {
        // check service readiness
        self.ready().await?;

        // call service
        self.svc
            .as_ref()
            .call(req, ServiceCtx::new(&self.waiters))
            .await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call_static<R>(&self, req: R) -> PipelineCall<S, R>
    where
        S: Service<R> + 'static,
    {
        PipelineCall {
            state: PipelineCallState::Ready { req: Some(req) },
            pipeline: self.clone(),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait<R>(&self, req: R) -> PipelineCall<S, R>
    where
        S: Service<R> + 'static,
    {
        PipelineCall {
            state: PipelineCallState::new_call(self, req),
            pipeline: self.clone(),
        }
    }

    /// Extract service if container hadnt been cloned before.
    pub fn into_service(self) -> Option<S> {
        let svc = self.svc.clone();
        drop(self);
        Rc::try_unwrap(svc).ok()
    }
}

impl<S> From<S> for Pipeline<S> {
    #[inline]
    fn from(svc: S) -> Self {
        Pipeline::new(svc)
    }
}

impl<S> Clone for Pipeline<S> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            svc: self.svc.clone(),
            pending: Cell::new(false),
            waiters: self.waiters.clone(),
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

#[allow(missing_debug_implementations)]
#[must_use = "futures do nothing unless polled"]
pub struct PipelineCall<S, R>
where
    S: Service<R>,
    R: 'static,
{
    state: PipelineCallState<S, R>,
    pipeline: Pipeline<S>,
}

impl<S: Service<R>, R> Unpin for PipelineCall<S, R> {}

enum PipelineCallState<S, Req>
where
    S: Service<Req>,
    Req: 'static,
{
    Ready {
        req: Option<Req>,
    },
    Call {
        fut: BoxFuture<'static, Result<S::Response, S::Error>>,
    },
}

impl<S, R> PipelineCallState<S, R>
where
    S: Service<R>,
    R: 'static,
{
    fn new_call<'a>(pl: &'a Pipeline<S>, req: R) -> Self {
        let ctx = ServiceCtx::new(&pl.waiters);
        let svc_call: BoxFuture<'a, Result<S::Response, S::Error>> =
            Box::pin(pl.get_ref().call(req, ctx));

        // SAFETY: `svc_call` has same lifetime same as lifetime of `pl.svc`
        // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
        // `svc_call` get resolved to result
        let fut = unsafe { std::mem::transmute(svc_call) };

        PipelineCallState::Call { fut }
    }
}

impl<S, R> Future for PipelineCall<S, R>
where
    S: Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut slf = self.as_mut();

        if let PipelineCallState::Call { ref mut fut, .. } = slf.state {
            return Pin::new(fut).poll(cx);
        }

        task::ready!(slf.pipeline.poll_ready(cx))?;

        let req = if let PipelineCallState::Ready { ref mut req } = slf.state {
            req.take().unwrap()
        } else {
            panic!("future must not be polled after it returned `Poll::Ready`")
        };
        slf.state = PipelineCallState::new_call(&slf.pipeline, req);
        slf.poll(cx)
    }
}

impl<S, R> fmt::Debug for PipelineCall<S, R>
where
    S: Service<R>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineCall").finish()
    }
}
