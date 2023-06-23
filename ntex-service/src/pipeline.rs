use std::{cell::Cell, future, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::ctx::{ServiceCall, ServiceCtx, Waiters};
use crate::{Service, ServiceFactory};

/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<S> {
    svc: Rc<S>,
    waiters: Waiters,
    pending: Cell<bool>,
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
    pub fn service_call<'a, R>(&'a self, req: R) -> ServiceCall<'a, S, R>
    where
        S: Service<R>,
    {
        ServiceCtx::<'a, S>::new(&self.waiters).call(self.svc.as_ref(), req)
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call<R>(&self, req: R) -> PipelineCall<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        let pipeline = self.clone();
        let svc_call = pipeline.svc.call(req, ServiceCtx::new(&pipeline.waiters));

        // SAFETY: `svc_call` has same lifetime same as lifetime of `pipeline.svc`
        // Pipeline::svc is heap allocated(Rc<S>), we keep it alive until
        // `svc_call` get resolved to result
        let fut = unsafe { std::mem::transmute(svc_call) };
        PipelineCall { fut, pipeline }
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

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct PipelineCall<S, R>
    where
        S: Service<R>,
        S: 'static,
        R: 'static,
    {
        #[pin]
        fut: S::Future<'static>,
        pipeline: Pipeline<S>,
    }
}

impl<S, R> future::Future for PipelineCall<S, R>
where
    S: Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().fut.poll(cx)
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct CreatePipeline<'f, F, R, C>
    where F: ServiceFactory<R, C>,
          F: ?Sized,
          F: 'f,
          C: 'f,
    {
        #[pin]
        fut: F::Future<'f>,
    }
}

impl<'f, F, R, C> CreatePipeline<'f, F, R, C>
where
    F: ServiceFactory<R, C> + 'f,
{
    pub(crate) fn new(fut: F::Future<'f>) -> Self {
        Self { fut }
    }
}

impl<'f, F, R, C> future::Future for CreatePipeline<'f, F, R, C>
where
    F: ServiceFactory<R, C> + 'f,
{
    type Output = Result<Pipeline<F::Service>, F::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(Pipeline::new(std::task::ready!(self
            .project()
            .fut
            .poll(cx))?)))
    }
}
