use std::{cell, fmt, future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use crate::{ctx::WaitersRef, Service, ServiceCtx};

#[derive(Debug)]
/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Pipeline<S> {
    index: u32,
    state: Rc<PipelineState<S>>,
}

struct PipelineState<S> {
    svc: S,
    waiters: WaitersRef,
}

impl<S> PipelineState<S> {
    pub(crate) fn waiters_ref(&self) -> &WaitersRef {
        &self.waiters
    }
}

impl<S> Pipeline<S> {
    #[inline]
    /// Construct new container instance.
    pub fn new(svc: S) -> Self {
        let (index, waiters) = WaitersRef::new();
        Pipeline {
            index,
            state: Rc::new(PipelineState { svc, waiters }),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.state.svc
    }

    #[inline]
    /// Returns when the pipeline is able to process requests.
    pub async fn ready<R>(&self) -> Result<(), S::Error>
    where
        S: Service<R>,
    {
        ServiceCtx::<'_, S>::new(self.index, self.state.waiters_ref())
            .ready(&self.state.svc)
            .await
    }

    #[inline]
    /// Returns when the pipeline is not able to process requests.
    pub async fn not_ready<R>(&self)
    where
        S: Service<R>,
    {
        self.state.svc.not_ready().await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub async fn call<R>(&self, req: R) -> Result<S::Response, S::Error>
    where
        S: Service<R>,
    {
        ServiceCtx::<'_, S>::new(self.index, self.state.waiters_ref())
            .call(&self.state.svc, req)
            .await
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call_static<R>(&self, req: R) -> PipelineCall<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(pl.index, pl.state.waiters_ref())
                    .call(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait<R>(&self, req: R) -> PipelineCall<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        let pl = self.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(pl.index, pl.state.waiters_ref())
                    .call_nowait(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Shutdown enclosed service.
    pub async fn shutdown<R>(&self)
    where
        S: Service<R>,
    {
        self.state.svc.shutdown().await
    }

    #[inline]
    /// Get current pipeline.
    pub fn bind<R>(self) -> PipelineBinding<S, R>
    where
        S: Service<R> + 'static,
        R: 'static,
    {
        PipelineBinding::new(self)
    }
}

impl<S> From<S> for Pipeline<S> {
    #[inline]
    fn from(svc: S) -> Self {
        Pipeline::new(svc)
    }
}

impl<S> Clone for Pipeline<S> {
    fn clone(&self) -> Self {
        Pipeline {
            index: self.state.waiters.insert(),
            state: self.state.clone(),
        }
    }
}

impl<S> Drop for Pipeline<S> {
    #[inline]
    fn drop(&mut self) {
        self.state.waiters.remove(self.index);
    }
}

impl<S: fmt::Debug> fmt::Debug for PipelineState<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineState")
            .field("svc", &self.svc)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

/// Bound container for a service.
pub struct PipelineBinding<S, R>
where
    S: Service<R>,
{
    pl: Pipeline<S>,
    st: cell::UnsafeCell<State<S::Error>>,
    not_ready: cell::UnsafeCell<StateNotReady>,
}

enum State<E> {
    New,
    Readiness(Pin<Box<dyn Future<Output = Result<(), E>> + 'static>>),
    Shutdown(Pin<Box<dyn Future<Output = ()> + 'static>>),
}

enum StateNotReady {
    New,
    Readiness(Pin<Box<dyn Future<Output = ()>>>),
}

impl<S, R> PipelineBinding<S, R>
where
    S: Service<R> + 'static,
    R: 'static,
{
    fn new(pl: Pipeline<S>) -> Self {
        PipelineBinding {
            pl,
            st: cell::UnsafeCell::new(State::New),
            not_ready: cell::UnsafeCell::new(StateNotReady::New),
        }
    }

    #[inline]
    /// Return reference to enclosed service
    pub fn get_ref(&self) -> &S {
        &self.pl.state.svc
    }

    #[inline]
    /// Returns `Ready` when the pipeline is able to process requests.
    ///
    /// panics if .poll_shutdown() was called before.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        let st = unsafe { &mut *self.st.get() };

        match st {
            State::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl: &'static Pipeline<S> = unsafe { std::mem::transmute(&self.pl) };
                let fut = Box::pin(CheckReadiness {
                    fut: None,
                    f: ready,
                    pl,
                });
                *st = State::Readiness(fut);
                self.poll_ready(cx)
            }
            State::Readiness(ref mut fut) => Pin::new(fut).poll(cx),
            State::Shutdown(_) => panic!("Pipeline is shutding down"),
        }
    }

    #[inline]
    /// Returns when the pipeline is not able to process requests.
    pub fn poll_not_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let st = unsafe { &mut *self.not_ready.get() };

        match st {
            StateNotReady::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl: &'static Pipeline<S> = unsafe { std::mem::transmute(&self.pl) };
                let fut = Box::pin(CheckUnReadiness {
                    fut: None,
                    f: not_ready,
                    pl,
                });
                *st = StateNotReady::Readiness(fut);
                self.poll_not_ready(cx)
            }
            StateNotReady::Readiness(ref mut fut) => Pin::new(fut).poll(cx),
        }
    }

    #[inline]
    /// Returns `Ready` when the service is properly shutdowns.
    pub fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let st = unsafe { &mut *self.st.get() };

        match st {
            State::New | State::Readiness(_) => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl: &'static Pipeline<S> = unsafe { std::mem::transmute(&self.pl) };
                *st = State::Shutdown(Box::pin(async move { pl.shutdown().await }));
                self.poll_shutdown(cx)
            }
            State::Shutdown(ref mut fut) => Pin::new(fut).poll(cx),
        }
    }

    #[inline]
    /// Wait for service readiness and then create future object
    /// that resolves to service result.
    pub fn call(&self, req: R) -> PipelineCall<S, R> {
        let pl = self.pl.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(pl.index, pl.state.waiters_ref())
                    .call(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Call service and create future object that resolves to service result.
    ///
    /// Note, this call does not check service readiness.
    pub fn call_nowait(&self, req: R) -> PipelineCall<S, R> {
        let pl = self.pl.clone();

        PipelineCall {
            fut: Box::pin(async move {
                ServiceCtx::<S>::new(pl.index, pl.state.waiters_ref())
                    .call_nowait(&pl.state.svc, req)
                    .await
            }),
        }
    }

    #[inline]
    /// Shutdown enclosed service.
    pub async fn shutdown(&self) {
        self.pl.state.svc.shutdown().await
    }
}

impl<S, R> Drop for PipelineBinding<S, R>
where
    S: Service<R>,
{
    fn drop(&mut self) {
        self.st = cell::UnsafeCell::new(State::New);
    }
}

impl<S, R> Clone for PipelineBinding<S, R>
where
    S: Service<R>,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            pl: self.pl.clone(),
            st: cell::UnsafeCell::new(State::New),
            not_ready: cell::UnsafeCell::new(StateNotReady::New),
        }
    }
}

impl<S, R> fmt::Debug for PipelineBinding<S, R>
where
    S: Service<R> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineBinding")
            .field("pipeline", &self.pl)
            .finish()
    }
}

#[must_use = "futures do nothing unless polled"]
/// Pipeline call future
pub struct PipelineCall<S, R>
where
    S: Service<R>,
    R: 'static,
{
    fut: Call<S::Response, S::Error>,
}

type Call<R, E> = Pin<Box<dyn Future<Output = Result<R, E>> + 'static>>;

impl<S, R> Future for PipelineCall<S, R>
where
    S: Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.as_mut().fut).poll(cx)
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

fn ready<S, R>(pl: &'static Pipeline<S>) -> impl Future<Output = Result<(), S::Error>>
where
    S: Service<R>,
    R: 'static,
{
    pl.state
        .svc
        .ready(ServiceCtx::<'_, S>::new(pl.index, pl.state.waiters_ref()))
}

fn not_ready<S, R>(pl: &'static Pipeline<S>) -> impl Future<Output = ()>
where
    S: Service<R>,
    R: 'static,
{
    pl.state.svc.not_ready()
}

struct CheckReadiness<S: 'static, F, Fut> {
    f: F,
    fut: Option<Fut>,
    pl: &'static Pipeline<S>,
}

impl<S, F, Fut> Unpin for CheckReadiness<S, F, Fut> {}

impl<S, F, Fut> Drop for CheckReadiness<S, F, Fut> {
    fn drop(&mut self) {
        // future fot dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.state.waiters.notify();
        }
    }
}

impl<T, S, F, Fut> Future for CheckReadiness<S, F, Fut>
where
    F: Fn(&'static Pipeline<S>) -> Fut,
    Fut: Future<Output = T>,
{
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slf = self.as_mut();

        if slf.pl.state.waiters.can_check(slf.pl.index, cx) {
            if let Some(ref mut fut) = slf.fut {
                match unsafe { Pin::new_unchecked(fut) }.poll(cx) {
                    Poll::Pending => {
                        slf.pl.state.waiters.register(slf.pl.index, cx);
                        Poll::Pending
                    }
                    Poll::Ready(res) => {
                        let _ = slf.fut.take();
                        slf.pl.state.waiters.notify();
                        Poll::Ready(res)
                    }
                }
            } else {
                slf.fut = Some((slf.f)(slf.pl));
                self.poll(cx)
            }
        } else {
            Poll::Pending
        }
    }
}

struct CheckUnReadiness<S: 'static, F, Fut> {
    f: F,
    fut: Option<Fut>,
    pl: &'static Pipeline<S>,
}

impl<S, F, Fut> Unpin for CheckUnReadiness<S, F, Fut> {}

impl<T, S, F, Fut> Future for CheckUnReadiness<S, F, Fut>
where
    F: Fn(&'static Pipeline<S>) -> Fut,
    Fut: Future<Output = T>,
{
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut slf = self.as_mut();

        if let Some(ref mut fut) = slf.fut {
            match unsafe { Pin::new_unchecked(fut) }.poll(cx) {
                Poll::Pending => {
                    slf.pl.state.waiters.register_unready(cx);
                    Poll::Pending
                }
                Poll::Ready(res) => {
                    let _ = slf.fut.take();
                    slf.pl.state.waiters.notify();
                    Poll::Ready(res)
                }
            }
        } else {
            slf.fut = Some((slf.f)(slf.pl));
            self.poll(cx)
        }
    }
}
