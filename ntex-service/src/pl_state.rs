use std::{cell, fmt, future, pin::Pin, ptr, rc::Rc, task::Context, task::Poll};

use crate::{Ctx, IntoService, Service, ctx::WaitersRef, util::BoxFuture};

use crate::pipeline::PipelineBinding;
use crate::pl_inner::{PipelineApi, PipelineInternalApi};

/// Execution container for a service whose state is supplied per operation.
///
/// Unlike [`crate::Pipeline`], this type does not own a state value. Callers
/// provide a state reference when checking readiness, calling, or shutting
/// down the service.
pub struct PipelineState<St, Req, Res, Err> {
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineState<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Creates a state-independent pipeline containing `service`.
    pub fn new<S>(service: impl IntoService<S, St, Req>) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
    {
        PipelineState {
            api: Rc::new(PipelineInner {
                s: service.into_service(),
                waiters: WaitersRef::new(),
                st_runtime: cell::UnsafeCell::new(RuntimeState::New),
            }),
        }
    }

    #[inline]
    /// Returns when the pipeline is ready to process requests.
    ///
    /// A successful check is consumed by the next call, which then skips
    /// its own readiness check.
    pub async fn ready(&self, st: &St) -> Result<(), Err> {
        self.api.ready(0, st).await
    }

    #[inline]
    /// Waits for readiness, then calls the service with `st`.
    ///
    /// The readiness check is skipped if the last pipeline readiness check
    /// succeeded and no call has started since.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = self.binding();
        self.api.call(pl.idx, req, st).await
    }

    #[inline]
    /// Shuts down the enclosed service.
    pub async fn shutdown(&self, st: &St) {
        self.api.shutdown(0, st).await;
    }

    #[inline]
    /// Returns `Ready` when the pipeline is ready to process requests.
    ///
    /// A successful check is consumed by the next call, which then skips
    /// its own readiness check.
    ///
    /// # Panics
    ///
    /// Panics if `.shutdown()` has been called. Unlike [`crate::Pipeline::poll_ready`],
    /// it does not return `Ready(Ok(()))` after shutdown.
    pub fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone,
    {
        self.api.poll_ready(cx, st)
    }

    fn binding(&self) -> Binding<'_, St, Req, Res, Err> {
        Binding {
            idx: self.api.reg(),
            api: self.api.as_ref(),
        }
    }

    #[inline]
    /// Creates a binding that accepts state per call.
    ///
    /// The binding can be used to call the service.
    pub fn bind(&self) -> PipelineStateBinding<St, Req, Res, Err> {
        PipelineStateBinding {
            idx: self.api.reg(),
            api: self.api.clone(),
        }
    }

    #[inline]
    /// Creates a standard pipeline binding by attaching an owned state value.
    ///
    /// The binding can be used to call the service.
    pub fn bind_state(&self, st: St) -> PipelineBinding<Req, Res, Err>
    where
        St: Clone,
    {
        let internal = PipelineInternal {
            st,
            api: self.api.clone(),
        };

        PipelineBinding::with(self.api.reg(), PipelineApi::with(internal))
    }
}

impl<St, Req, Res, Err> Drop for PipelineState<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(0);
    }
}

struct Binding<'a, St, Req, Res, Err> {
    idx: u32,
    api: &'a dyn PipelineStateApi<St, Req, Res, Err>,
}

impl<St, Req, Res, Err> Drop for Binding<'_, St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(self.idx);
    }
}

// ========================== `PipelineStateBinding` ===========================

/// An independently registered handle to a [`PipelineState`].
pub struct PipelineStateBinding<St, Req, Res, Err> {
    idx: u32,
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> Drop for PipelineStateBinding<St, Req, Res, Err> {
    #[inline]
    fn drop(&mut self) {
        self.api.unreg(self.idx);
    }
}

impl<St, Req, Res, Err> Clone for PipelineStateBinding<St, Req, Res, Err> {
    #[inline]
    fn clone(&self) -> Self {
        PipelineStateBinding {
            idx: self.api.reg(),
            api: self.api.clone(),
        }
    }
}

impl<St, Req, Res, Err> PipelineStateBinding<St, Req, Res, Err>
where
    St: 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    #[inline]
    /// Waits for readiness, then calls the service with `st`.
    ///
    /// The readiness check is skipped if the last pipeline readiness check
    /// succeeded and no call has started since.
    pub async fn call(&self, req: Req, st: &St) -> Result<Res, Err> {
        let pl = Binding {
            idx: self.api.reg(),
            api: self.api.as_ref(),
        };
        pl.api.call(pl.idx, req, st).await
    }
}

// ========================== `PipelineApi` ===========================

struct PipelineInternal<St, Req, Res, Err> {
    st: St,
    api: Rc<dyn PipelineStateApi<St, Req, Res, Err>>,
}

impl<St, Req, Res, Err> PipelineInternalApi<Req, Res, Err> for PipelineInternal<St, Req, Res, Err> {
    fn reg(&self) -> u32 {
        self.api.reg()
    }

    fn unreg(&self, idx: u32) {
        self.api.unreg(idx);
    }

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>> {
        self.api.ready(idx, &self.st)
    }

    fn call(&self, idx: u32, req: Req) -> BoxFuture<'_, Result<Res, Err>> {
        self.api.call(idx, req, &self.st)
    }

    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Err>> {
        unreachable!()
    }

    fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
        unreachable!()
    }

    fn is_shutdown(&self) -> bool {
        self.api.is_shutdown()
    }
}

// ========================== `PipelineStateApi` ===========================

struct PipelineInner<S, St, E> {
    s: S,
    waiters: WaitersRef,
    st_runtime: cell::UnsafeCell<RuntimeState<St, E>>,
}

impl<S, St, E> Drop for PipelineInner<S, St, E> {
    fn drop(&mut self) {
        // The readiness future borrows `s` and `waiters`, so it must be dropped
        // before the fields it references
        *self.st_runtime.get_mut() = RuntimeState::New;
    }
}

enum RuntimeState<St, E> {
    New,
    Readiness(Box<dyn CheckReadiness<St, E>>),
    Shutdown,
}

trait PipelineStateApi<St, Req, Res, Err> {
    fn reg(&self) -> u32;
    fn unreg(&self, idx: u32);

    fn call<'a>(&'a self, idx: u32, req: Req, st: &'a St) -> BoxFuture<'a, Result<Res, Err>>
    where
        Req: 'a;

    fn ready<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, Result<(), Err>>
    where
        Req: 'a;

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), Err>>
    where
        St: Clone;

    fn shutdown<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, ()>;

    fn is_shutdown(&self) -> bool;
}

impl<S, St, Req, E> PipelineStateApi<St, Req, S::Res, S::Error> for PipelineInner<S, St, E>
where
    S: Service<St, Req, Error = E> + 'static,
    St: 'static,
    Req: 'static,
    E: 'static,
{
    fn reg(&self) -> u32 {
        self.waiters.insert()
    }

    fn unreg(&self, idx: u32) {
        self.waiters.remove(idx);
    }

    fn ready<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, Result<(), S::Error>>
    where
        Req: 'a,
    {
        Box::pin(async move {
            self.waiters.set_ready(false);
            let result = Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                .ready(&self.s)
                .await;
            self.waiters.set_ready(result.is_ok());
            result
        })
    }

    fn shutdown<'a>(&'a self, idx: u32, st: &'a St) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let pl_state = unsafe { &mut *self.st_runtime.get() };
            *pl_state = RuntimeState::Shutdown;
            self.waiters.set_ready(false);

            Ctx::<'_, S, St>::new(idx, &self.waiters, st)
                .shutdown(&self.s)
                .await;
        })
    }

    fn call<'a>(&'a self, idx: u32, req: Req, st: &'a St) -> BoxFuture<'a, Result<S::Res, S::Error>>
    where
        Req: 'a,
    {
        Box::pin(async move {
            let ctx = Ctx::<'_, S, St>::new(idx, &self.waiters, st);
            if !self.waiters.take_ready() {
                let result = ctx.ready(&self.s).await;
                // the call consumes any readiness reported while it was waiting
                self.waiters.set_ready(false);
                result?;
            }
            ctx.call_nowait(&self.s, req).await
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>>
    where
        St: Clone,
    {
        let pl_state = unsafe { &mut *self.st_runtime.get() };
        match pl_state {
            RuntimeState::New => {
                // SAFETY: `self` is heap allocated (`Rc<PipelineInner>`) and never moves.
                // `fut` is stored in `self.st_runtime`, which is reset before other
                // fields are dropped (see `Drop for PipelineInner`), so `pl` outlives `fut`.
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
            RuntimeState::Shutdown => panic!("Pipeline is shutting down"),
        }
    }

    fn is_shutdown(&self) -> bool {
        self.waiters.is_shutdown()
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
    pl: &'static PipelineInner<S, St, S::Error>,
}

fn ready<S, St, Req>(
    st: &'static St,
    pl: &'static PipelineInner<S, St, S::Error>,
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
    F: Fn(&'static St, &'static PipelineInner<S, St, S::Error>) -> Fut,
    Fut: Future<Output = Result<(), S::Error>>,
{
    fn poll(&mut self, cx: &mut Context<'_>, st: &St) -> Poll<Result<(), S::Error>> {
        let result = self.pl.waiters.run(0, cx, |cx| {
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
        });
        self.pl
            .waiters
            .set_ready(matches!(result, Poll::Ready(Ok(()))));
        result
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineState<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineState").finish()
    }
}

impl<St, Req, Res, Err> fmt::Debug for PipelineStateBinding<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PipelineStateBinding").finish()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, future::pending, task::Waker};

    use ntex::{channel::condition, util::lazy};

    use super::*;

    struct Srv(Rc<Cell<usize>>, condition::Waiter);

    impl Service<usize, usize> for Srv {
        type Res = usize;
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), ()> {
            self.0.set(self.0.get() + 1);
            self.1.ready().await;
            Ok(())
        }

        async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, ()> {
            if req == 0 { Err(()) } else { Ok(req + *ctx.st()) }
        }

        async fn shutdown(&self, ctx: Ctx<'_, Self, usize>) {
            self.0.set(self.0.get() + 100 * *ctx);
        }
    }

    #[ntex::test]
    async fn pipeline_state() {
        let cnt = Rc::new(Cell::new(0));
        let cond = condition::Condition::new();
        let pl = PipelineState::new(Srv(cnt.clone(), cond.wait()));
        assert!(format!("{pl:?}").contains("PipelineState"));

        cond.notify_and_lock(());
        assert_eq!(pl.ready(&1).await, Ok(()));
        assert_eq!(cnt.get(), 1);
        // the successful readiness check is consumed by the next call
        assert_eq!(pl.call(1, &2).await, Ok(3));
        assert_eq!(cnt.get(), 1);
        assert_eq!(pl.call(0, &2).await, Err(()));
        assert_eq!(pl.call(2, &3).await, Ok(5));
        assert_eq!(cnt.get(), 3);

        let b = pl.bind();
        assert!(format!("{b:?}").contains("PipelineStateBinding"));
        let b2 = b.clone();
        drop(b);
        assert_eq!(b2.call(1, &10).await, Ok(11));
        assert_eq!(cnt.get(), 4);
        assert_eq!(pl.ready(&1).await, Ok(()));
        assert_eq!(b2.call(1, &20).await, Ok(21));
        assert_eq!(cnt.get(), 5);

        let b = pl.bind_state(7);
        assert_eq!(b.ready().await, Ok(()));
        assert_eq!(b.call(1).await, Ok(8));
        assert_eq!(cnt.get(), 6);
        assert_eq!(b.call(2).await, Ok(9));
        assert_eq!(b.clone().call_static(3).await, Ok(10));
        assert_eq!(cnt.get(), 8);
        drop(b);

        assert_eq!(lazy(|cx| pl.poll_ready(cx, &1)).await, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 9);
        assert_eq!(pl.call(1, &2).await, Ok(3));
        assert_eq!(cnt.get(), 9);

        // shutdown resets the flag
        assert_eq!(pl.ready(&1).await, Ok(()));
        pl.shutdown(&2).await;
        assert_eq!(cnt.get(), 210);
        assert_eq!(pl.call(1, &2).await, Ok(3));
        assert_eq!(cnt.get(), 211);
    }

    #[ntex::test]
    async fn pipeline_state_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cond = condition::Condition::new();
        let pl = PipelineState::new(Srv(cnt.clone(), cond.wait()));

        assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
        assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
        assert_eq!(cnt.get(), 1);

        // binding waits while main readiness check is in progress
        let b = pl.bind_state(1);
        let mut fut = Box::pin(b.ready());
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        assert_eq!(cnt.get(), 1);

        cond.notify(());
        assert_eq!(lazy(|cx| pl.poll_ready(cx, &1)).await, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 1);

        // binding owns the readiness check now
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        assert_eq!(cnt.get(), 2);
        assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
        assert_eq!(cnt.get(), 2);

        // dropping the owner releases the readiness check
        drop(fut);
        assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
        assert_eq!(cnt.get(), 3);
    }

    #[ntex::test]
    async fn pipeline_state_ready_flag() {
        let cnt = Rc::new(Cell::new(0));
        let cond = condition::Condition::new();
        let pl = PipelineState::new(Srv(cnt.clone(), cond.wait()));

        let mut fut = Box::pin(pl.ready(&1));
        assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
        cond.notify(());
        assert_eq!(fut.await, Ok(()));
        assert_eq!(cnt.get(), 1);

        // a pending readiness check clears the flag
        assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
        assert_eq!(cnt.get(), 2);
        let mut call = Box::pin(pl.call(1, &2));
        assert!(lazy(|cx| call.as_mut().poll(cx)).await.is_pending());
        assert_eq!(cnt.get(), 2);

        cond.notify(());
        assert_eq!(lazy(|cx| pl.poll_ready(cx, &1)).await, Poll::Ready(Ok(())));

        // the waiting call checks readiness itself
        assert!(lazy(|cx| call.as_mut().poll(cx)).await.is_pending());
        assert_eq!(cnt.get(), 3);
        cond.notify(());
        assert_eq!(lazy(|cx| call.as_mut().poll(cx)).await, Poll::Ready(Ok(3)));
        assert_eq!(cnt.get(), 3);

        // readiness reported while the call was waiting is consumed by it
        let mut call = Box::pin(pl.call(1, &2));
        assert!(lazy(|cx| call.as_mut().poll(cx)).await.is_pending());
        assert_eq!(cnt.get(), 4);
    }

    #[ntex::test]
    #[should_panic(expected = "Pipeline is shutting down")]
    async fn pipeline_state_poll_ready_after_shutdown() {
        let cond = condition::Condition::new();
        let pl = PipelineState::new(Srv(Rc::default(), cond.wait()));
        pl.shutdown(&1).await;
        let _ = lazy(|cx| pl.poll_ready(cx, &1)).await;
    }

    struct Guard<'a>(&'a [usize]);

    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            assert_eq!(self.0.iter().sum::<usize>(), 3);
        }
    }

    struct Pending(Vec<usize>);

    impl Service<usize, ()> for Pending {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), ()> {
            let _g = Guard(&self.0);
            pending().await
        }

        async fn call(&self, (): (), _: Ctx<'_, Self, usize>) -> Result<(), ()> {
            Ok(())
        }
    }

    #[test]
    fn miri_drop_with_pending_readiness() {
        let mut cx = Context::from_waker(Waker::noop());

        let pl = PipelineState::new(Pending(vec![1, 2]));
        assert!(pl.poll_ready(&mut cx, &1).is_pending());
        drop(pl);
    }
}
