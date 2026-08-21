use std::{cell, fmt, future, mem, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, IntoService, Service, ctx::WaitersRef, util::BoxFuture};

/// Container for a service.
///
/// Provides a way to call the enclosed service and share its readiness state.
pub struct PipelineWithState<St, Req, Res, Err> {
    state: Rc<dyn PipelineApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineWithState<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Construct new service pipeline instance with default state.
    pub fn new<S>(f: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
    {
        PipelineWithState {
            state: Rc::new(PipelineState {
                s: f.into_service(),
                waiters: WaitersRef::new(),
                st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            }),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self, st: &St) -> Result<(), Err>
    where
        St: Clone,
    {
        future::poll_fn(|cx| self.poll_ready(cx, st)).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.binding();
        self.state.call(pl.index, req, st, true).await
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub async fn call_nowait(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.binding();
        pl.state.call(pl.index, req, st, false).await
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self, st: &St) {
        // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
        // it is being kept alive until `self` is alive
        let st = unsafe { mem::transmute::<&St, &St>(st) };
        future::poll_fn(|cx| self.state.poll_shutdown(cx, st)).await;
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// # Panics
    ///
    /// Panics if the pipeline is shutting down (i.e., `.shutdown()` or
    /// `.poll_shutdown()` has been called).
    pub fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone,
    {
        self.state.poll_ready(cx, st)
    }

    fn binding(&self) -> Binding<'_, St, Req, Res, Err> {
        Binding {
            index: self.state.reg(),
            state: self.state.as_ref(),
        }
    }

    #[inline]
    /// Returns the current pipeline binding.
    ///
    /// The binding can be used to call the service.
    pub fn bind(&self) -> PipelineWithStateBinding<St, Req, Res, Err> {
        PipelineWithStateBinding {
            index: self.state.reg(),
            state: self.state.clone(),
        }
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineWithState<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineWithState").finish()
    }
}

impl<St, Req, Res, Err> Drop for PipelineWithState<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.state.unreg(0);
    }
}

struct Binding<'a, St, Req, Res, Err> {
    index: u32,
    state: &'a dyn PipelineApi<St, Req, Res, Err>,
}

impl<St, Req, Res, Err> Drop for Binding<'_, St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.state.unreg(self.index);
    }
}

pub struct PipelineWithStateBinding<St, Req, Res, Err> {
    index: u32,
    state: Rc<dyn PipelineApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> Clone for PipelineWithStateBinding<St, Req, Res, Err> {
    #[inline]
    fn clone(&self) -> Self {
        PipelineWithStateBinding {
            index: self.state.reg(),
            state: self.state.clone(),
        }
    }
}

impl<St, Req, Res, Err> Drop for PipelineWithStateBinding<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.state.unreg(self.index);
    }
}

impl<St, Req, Res, Err> PipelineWithStateBinding<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.clone();
        self.state.call(pl.index, req, st, true).await
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub async fn call_nowait(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.clone();
        pl.state.call(pl.index, req, st, false).await
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineWithStateBinding<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineWithStateBinding").finish()
    }
}

struct PipelineState<S, St, E> {
    s: S,
    waiters: WaitersRef,
    st_runtime: cell::UnsafeCell<RuntimeState<St, E>>,
}

enum RuntimeState<St, E> {
    New,
    Readiness(Box<dyn CheckReadiness<St, E>>),
    Shutdown(BoxFuture<'static, ()>),
}

trait PipelineApi<St, Req, Res, Err> {
    fn reg(&self) -> u32;
    fn unreg(&self, idx: u32);

    fn call<'a>(
        &'a self,
        idx: u32,
        req: Req,
        st: &'a St,
        ready: bool,
    ) -> BoxFuture<'a, Result<Res, Err>>
    where
        Req: 'a;

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone;

    fn poll_shutdown(&self, cx: &mut Context<'_>, st: &'static St) -> Poll<()>;
}

impl<S, St, Req, E> PipelineApi<St, Req, S::Res, S::Error> for PipelineState<S, St, E>
where
    S: Service<St, Req, Error = E> + 'static,
    St: 'static,
    Req: 'static,
    E: 'static,
{
    fn reg(&self) -> u32 {
        self.waiters.insert()
    }

    fn unreg(&self, index: u32) {
        self.waiters.remove(index);
    }

    fn call<'a>(
        &'a self,
        idx: u32,
        req: Req,
        st: &'a St,
        ready: bool,
    ) -> BoxFuture<'a, Result<S::Res, S::Error>>
    where
        Req: 'a,
    {
        Box::pin(async move {
            if ready {
                Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                    .call(&self.s, req)
                    .await
            } else {
                Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                    .call_nowait(&self.s, req)
                    .await
            }
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>>
    where
        St: Clone,
    {
        let pl_state = unsafe { &mut *self.st_runtime.get() };
        match pl_state {
            RuntimeState::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(self)) };
                let fut = Box::new(CheckReadinessFut {
                    pl,
                    f: ready,
                    st: st.clone(),
                    fut: None,
                });
                *pl_state = RuntimeState::Readiness(fut);
                self.poll_ready(cx, st)
            }
            RuntimeState::Readiness(fut) => fut.poll(cx, st),
            RuntimeState::Shutdown(_) => panic!("Pipeline is shutding down"),
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>, st: &'static St) -> Poll<()> {
        let pl_state = unsafe { &mut *self.st_runtime.get() };
        match pl_state {
            RuntimeState::New | RuntimeState::Readiness(_) => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(self)) };
                let fut = Box::pin(async move {
                    Ctx::<'_, S, St>::new(0, &pl.waiters, st)
                        .shutdown(&pl.s)
                        .await;
                });
                *pl_state = RuntimeState::Shutdown(fut);
                pl.waiters.shutdown();
                self.poll_shutdown(cx, st)
            }
            RuntimeState::Shutdown(fut) => Pin::new(fut).poll(cx),
        }
    }
}

trait CheckReadiness<St, E> {
    fn poll(&mut self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), E>>;
}

struct CheckReadinessFut<S, St, Req, F, Fut>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    f: F,
    st: St,
    fut: Option<Fut>,
    pl: &'static PipelineState<S, St, S::Error>,
}

fn ready<S, St, Req>(
    st: &'static St,
    pl: &'static PipelineState<S, St, S::Error>,
) -> impl future::Future<Output = Result<(), S::Error>>
where
    S: Service<St, Req>,
{
    pl.s.ready(Ctx::<'_, S, St>::new(0, &pl.waiters, st))
}

impl<S: Service<St, Req>, St, Req, F, Fut> Drop for CheckReadinessFut<S, St, Req, F, Fut> {
    fn drop(&mut self) {
        // future got dropped during polling, we must notify other waiters
        if self.fut.is_some() {
            self.pl.waiters.notify();
        }
    }
}

impl<S, St, Req, F, Fut> CheckReadiness<St, S::Error> for CheckReadinessFut<S, St, Req, F, Fut>
where
    St: Clone,
    S: Service<St, Req>,
    F: Fn(&'static St, &'static PipelineState<S, St, S::Error>) -> Fut,
    Fut: Future<Output = Result<(), S::Error>>,
{
    fn poll(&mut self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>> {
        self.pl.waiters.run(0, cx, |cx| {
            if self.fut.is_none() {
                self.st = st.clone();
                let st: &'static St = unsafe { std::mem::transmute(&self.st) };
                self.fut = Some((self.f)(st, self.pl));
            }
            let fut = self.fut.as_mut().unwrap();
            let result = unsafe { Pin::new_unchecked(fut) }.poll(cx);
            if result.is_ready() {
                let _ = self.fut.take();
            }
            result
        })
    }
}
