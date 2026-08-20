use std::{future::poll_fn, task::Context, task::Poll};

use crate::util::BoxFuture;

/// Container for a service.
///
/// Provides a way to call the enclosed service and share its readiness state.
pub struct PipelineNostate<St, Req, Res, Err> {
    state: Rc<dyn PipelineApi<Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineNostate<St, Req, Res, Err>
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
        PipelineNostate {
            state: Rc::new(PipelineState {
                s: f.into_service(),
                waiters: WaitersRef::new(),
                st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            }),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    pub async fn ready(&self, st: &St) -> Result<(), Err> {
        self.state.ready(0).await
    }

    #[inline]
    /// Wait for service readiness, then create a future
    /// that resolves to the service call result.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        self.state.call(0, req, true).await
    }

    #[inline]
    /// Call the service and create a future that resolves to the service result.
    ///
    /// This call can be completed from different async tasks.
    /// Note: this call does not check service readiness.
    pub async fn call_nowait(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.bind();
        PipelineCall(Box::pin(async move {
            pl.state.call(pl.index, req, false).await
        }))
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self) {
        poll_fn(|cx| self.state.poll_shutdown(cx)).await;
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
}

impl<St, Req, Res, Err> fmt::Debug for PipelineNostate<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineNostate").finish()
    }
}

impl<St, Req, Res, Err> Drop for PipelineNostate<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.state.unregister(0);
    }
}

struct PipelineState<S, St, E> {
    s: S,
    waiters: WaitersRef,
    st_runtime: cell::UnsafeCell<RuntimeState<E>>,
}

enum RuntimeState<E> {
    New,
    Readiness(Pin<Box<dyn Future<Output = Result<(), E>>>>),
    Shutdown(Pin<Box<dyn Future<Output = ()>>>),
}

trait PipelineApi<Req, Res, Err> {
    fn register(&self) -> u32;

    fn unregister(&self, idx: u32);

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>>;

    fn call(&self, idx: u32, req: Req, ready: bool) -> BoxFuture<'_, Result<Res, Err>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>;
}

impl<S, St, E> PipelineApi<Req, S::Res, S::Error> for PipelineState<S, St, E>
where
    S: Service<St, Req, Error = E> + 'static,
    St: 'static,
{
    fn register(&self) -> u32 {
        self.waiters.insert()
    }

    fn unregister(&self, index: u32) {
        self.waiters.remove(index);
    }

    fn ready(&self, idx: u32, st: &St) -> Pin<Box<dyn Future<Output = Result<(), S::Error>> + '_>> {
        Box::pin(async move {
            Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                .ready(&self.s)
                .await
        })
    }

    fn call(
        &self,
        idx: u32,
        req: Req,
        st: &St,
        ready: bool,
    ) -> Pin<Box<dyn Future<Output = Result<S::Res, S::Error>> + '_>> {
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
        let st = unsafe { &mut *self.st_runtime.get() };
        match st {
            RuntimeState::New => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(self)) };
                let fut = Box::pin(CheckReadiness {
                    pl,
                    f: ready,
                    fut: None,
                });
                *st = RuntimeState::Readiness(fut);
                self.poll_ready(cx)
            }
            RuntimeState::Readiness(fut) => Pin::new(fut).poll(cx),
            RuntimeState::Shutdown(_) => panic!("Pipeline is shutding down"),
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let st = unsafe { &mut *self.st_runtime.get() };
        match st {
            RuntimeState::New | RuntimeState::Readiness(_) => {
                // SAFETY: `fut` has same lifetime same as lifetime of `self.pl`.
                // Pipeline::svc is heap allocated(Rc<S>), and it is being kept alive until
                // `self` is alive
                let pl = unsafe { &*(ptr::from_ref(self)) };
                let fut = Box::pin(async move { pl.s.shutdown().await });
                *st = RuntimeState::Shutdown(fut);
                pl.waiters.shutdown();
                self.poll_shutdown(cx)
            }
            RuntimeState::Shutdown(fut) => Pin::new(fut).poll(cx),
        }
    }
}
