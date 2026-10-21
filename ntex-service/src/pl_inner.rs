use std::{
    cell::RefCell, future::Future, future::poll_fn, marker::PhantomPinned, mem, ptr, rc::Rc,
    task::Context, task::Poll, task::Waker,
};

use crate::readiness::{Generation, ReadyObserver, ReadyScope};
use crate::{BindCtx, Ctx, ReadyCallback, Service, ctx::WaitersRef, util::BoxFuture};

// Pipeline-owned checks can be driven by any waiting caller. Caller-owned
// checks stay in their caller's future, so cancelling it releases ownership.
pub(crate) struct PipelineReadiness<E> {
    scope: ReadyScope,
    observer: ReadyObserver,
    checker_waker: Waker,
    checker: RefCell<Checker<E>>,
}

enum Checker<E> {
    Idle,
    Caller,
    Pipeline(BoxFuture<'static, Result<Generation, E>>),
    Polling { cancelled: bool },
}

impl<E> PipelineReadiness<E> {
    pub(crate) fn new(scope: &ReadyScope) -> Self {
        Self {
            scope: scope.clone(),
            observer: scope.observer(),
            checker_waker: scope.checker_waker(),
            checker: RefCell::new(Checker::Idle),
        }
    }

    pub(crate) fn unregister(&self) {
        self.observer.clear();
    }

    pub(crate) fn cancel(&self) {
        let previous = {
            let mut checker = self.checker.borrow_mut();
            match &mut *checker {
                Checker::Polling { cancelled } => {
                    *cancelled = true;
                    None
                }
                Checker::Pipeline(_) => Some(std::mem::replace(
                    &mut *checker,
                    Checker::Polling { cancelled: true },
                )),
                Checker::Idle | Checker::Caller => None,
            }
        };
        self.scope.reset_root();
        if let Some(previous) = previous {
            drop(previous);
            self.checker.replace(Checker::Idle);
            self.scope.wake_observers();
        }
    }

    pub(crate) async fn ready<F, Fut>(&self, mut check: F) -> Result<(), E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Generation, E>>,
    {
        loop {
            if self.scope.is_ready() {
                return Ok(());
            }
            let mut owner = None;
            let observer = self.scope.observer();
            let result = poll_fn(|cx| {
                observer.register(cx.waker());
                if self.scope.is_ready() {
                    return Poll::Ready(Ok(()));
                }
                match self.poll_check() {
                    Poll::Ready(None) => {
                        self.checker.replace(Checker::Caller);
                        owner = Some(CallerCheck(self));
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Some(result)) => Poll::Ready(result),
                    Poll::Pending => Poll::Pending,
                }
            })
            .await;
            let result = if owner.is_some() {
                check().await.map(|generation| {
                    self.scope.set_ready(generation);
                })
            } else {
                result
            };
            drop(observer);
            drop(owner);
            result?;
        }
    }

    pub(crate) fn poll_ready(
        &self,
        cx: &mut Context<'_>,
        check: impl FnOnce() -> BoxFuture<'static, Result<Generation, E>>,
    ) -> Poll<Result<(), E>> {
        let mut check = Some(check);
        loop {
            self.observer.register(cx.waker());
            if self.scope.is_ready() {
                return Poll::Ready(Ok(()));
            }
            match self.poll_check() {
                Poll::Ready(None) => {
                    let Some(check) = check.take() else {
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    };
                    self.checker.replace(Checker::Polling { cancelled: false });
                    let mut guard = PipelineCheck {
                        readiness: self,
                        fut: None,
                    };
                    guard.fut = Some(check());
                    guard.retain();
                }
                Poll::Ready(Some(result)) => {
                    result?;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn poll_check(&self) -> Poll<Option<Result<(), E>>> {
        let previous = {
            let mut checker = self.checker.borrow_mut();
            match &*checker {
                Checker::Idle => return Poll::Ready(None),
                Checker::Caller | Checker::Polling { .. } => return Poll::Pending,
                Checker::Pipeline(_) => {
                    std::mem::replace(&mut *checker, Checker::Polling { cancelled: false })
                }
            }
        };
        let Checker::Pipeline(fut) = previous else {
            unreachable!()
        };
        let mut guard = PipelineCheck {
            readiness: self,
            fut: Some(fut),
        };
        let result = match guard
            .fut
            .as_mut()
            .unwrap()
            .as_mut()
            .poll(&mut Context::from_waker(&self.checker_waker))
        {
            Poll::Pending => {
                guard.retain();
                return Poll::Pending;
            }
            Poll::Ready(result) => result,
        };
        // Drop the future before handing ownership to another task.
        drop(guard.fut.take());
        if guard.cancelled() {
            return Poll::Pending;
        }
        let result = result.map(|generation| {
            self.scope.set_ready(generation);
        });
        drop(guard);
        Poll::Ready(Some(result))
    }
}

struct CallerCheck<'a, E>(&'a PipelineReadiness<E>);

impl<E> Drop for CallerCheck<'_, E> {
    fn drop(&mut self) {
        self.0.checker.replace(Checker::Idle);
        self.0.scope.wake_observers();
    }
}

struct PipelineCheck<'a, E> {
    readiness: &'a PipelineReadiness<E>,
    fut: Option<BoxFuture<'static, Result<Generation, E>>>,
}

impl<E> PipelineCheck<'_, E> {
    fn cancelled(&self) -> bool {
        matches!(
            *self.readiness.checker.borrow(),
            Checker::Polling { cancelled: true }
        )
    }

    fn retain(mut self) {
        if !self.cancelled() {
            self.readiness
                .checker
                .replace(Checker::Pipeline(self.fut.take().unwrap()));
            // The future and ownership have moved back into the pipeline.
            mem::forget(self);
        }
    }
}

impl<E> Drop for PipelineCheck<'_, E> {
    fn drop(&mut self) {
        drop(self.fut.take());
        self.readiness.checker.replace(Checker::Idle);
        self.readiness.scope.wake_observers();
    }
}

// ======================== PipelineApi ============================

pub(crate) struct PipelineApi<Req, Res, Err>(Rc<dyn PipelineInternalApi<Req, Res, Err>>);

impl<Req, Res, Err> PipelineApi<Req, Res, Err> {
    pub(crate) fn new<S, St>(s: S, st: St) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
        St: 'static,
        Req: 'static,
    {
        let waiters = WaitersRef::new();
        s.bind(BindCtx::new(waiters.scope()));
        PipelineApi(Rc::new(PipelineInner {
            s,
            st,
            readiness: PipelineReadiness::new(waiters.scope()),
            waiters,
            st_runtime: RefCell::new(RuntimeState::New),
            _pinned: PhantomPinned,
        }))
    }

    pub(crate) fn with(api: impl PipelineInternalApi<Req, Res, Err> + 'static) -> Self {
        Self(Rc::new(api))
    }
}

impl<Req, Res, Err> PipelineApi<Req, Res, Err> {
    pub(crate) fn reg(&self) -> u32 {
        self.0.reg()
    }

    pub(crate) fn unreg(&self, idx: u32) {
        self.0.unreg(idx);
    }

    pub(crate) fn bind_to(&self, ctx: &BindCtx<'_>) {
        ctx.depend_on(self.0.scope());
    }

    pub(crate) fn on_notify(&self, callback: &ReadyCallback) {
        self.0.scope().on_notify(callback);
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.0.scope().is_ready()
    }

    pub(crate) async fn ready(&self, idx: u32) -> Result<(), Err> {
        if self.is_ready() {
            Ok(())
        } else {
            self.0.ready(idx).await
        }
    }

    pub(crate) fn call(&self, idx: u32, req: Req) -> BoxFuture<'_, Result<Res, Err>> {
        self.0.call(idx, req)
    }

    pub(crate) fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>> {
        self.0.poll_ready(cx)
    }

    pub(crate) fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.0.poll_shutdown(cx)
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.0.is_shutdown()
    }
}

impl<Req, Res, Err> Clone for PipelineApi<Req, Res, Err> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

// ======================== PipelineInner ============================

struct PipelineInner<S: Service<St, Req>, St, Req> {
    s: S,
    st: St,
    readiness: PipelineReadiness<S::Error>,
    st_runtime: RefCell<RuntimeState>,
    waiters: WaitersRef,
    // The pending readiness future borrows the other fields. `!Unpin` keeps
    // `&mut self` in `Drop` from invalidating those borrows.
    _pinned: PhantomPinned,
}

impl<S: Service<St, Req>, St, Req> Drop for PipelineInner<S, St, Req> {
    fn drop(&mut self) {
        // Readiness and shutdown futures borrow `s`, `st` and `waiters`, so they
        // must be dropped before the fields they reference
        *self.st_runtime.get_mut() = RuntimeState::Done;
        self.readiness.cancel();
    }
}

enum RuntimeState {
    New,
    Polling,
    Shutdown(BoxFuture<'static, ()>),
    Done,
}

// ======================== PipelineInternalApi ============================

pub(crate) trait PipelineInternalApi<Req, Res, Err> {
    fn scope(&self) -> &ReadyScope;

    fn reg(&self) -> u32;

    fn unreg(&self, idx: u32);

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), Err>>;

    fn call(&self, idx: u32, req: Req) -> BoxFuture<'_, Result<Res, Err>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Err>>;

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()>;

    fn is_shutdown(&self) -> bool;
}

impl<S, St, Req> PipelineInternalApi<Req, S::Res, S::Error> for PipelineInner<S, St, Req>
where
    S: Service<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    fn scope(&self) -> &ReadyScope {
        self.waiters.scope()
    }

    fn reg(&self) -> u32 {
        self.waiters.insert()
    }

    fn unreg(&self, index: u32) {
        if index == 0 {
            self.readiness.unregister();
        }
        self.waiters.remove(index);
    }

    fn ready(&self, idx: u32) -> BoxFuture<'_, Result<(), S::Error>> {
        Box::pin(self.readiness.ready(move || async move {
            Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st)
                .ready_generation(&self.s)
                .await
        }))
    }

    fn call(&self, idx: u32, req: Req) -> BoxFuture<'_, Result<S::Res, S::Error>> {
        Box::pin(async move {
            let ctx = Ctx::<'_, S, St>::new(idx, &self.waiters, &self.st);
            self.readiness
                .ready(|| ctx.ready_generation(&self.s))
                .await?;
            self.s.call(req, ctx.into_call()).await
        })
    }

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        if self.waiters.is_shutdown() {
            return Poll::Ready(Ok(()));
        }
        self.readiness.poll_ready(cx, || {
            // SAFETY: `self` is heap allocated (`Rc<PipelineInner>`) and never moves.
            // `fut` is stored in `self.readiness`, which is reset before other
            // fields are dropped (see `Drop for PipelineInner`), so `pl` outlives `fut`.
            let pl = unsafe { &*(ptr::from_ref(self)) };
            Box::pin(async move {
                let registration = pl.waiters.register();
                Ctx::<'_, S, St>::new(registration.idx, &pl.waiters, &pl.st)
                    .ready_generation(&pl.s)
                    .await
            })
        })
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        let state = self.st_runtime.replace(RuntimeState::Polling);
        let mut fut = match state {
            RuntimeState::New => {
                self.waiters.shutdown();
                self.readiness.cancel();
                // SAFETY: `self` is heap allocated (`Rc<PipelineInner>`) and never moves.
                // `fut` is stored in `self.st_runtime`, which is reset before other
                // fields are dropped (see `Drop for PipelineInner`), so `pl` outlives `fut`.
                let pl = unsafe { &*(ptr::from_ref(self)) };

                Box::pin(async move {
                    let ctx = Ctx::<'_, S, St>::new(0, &pl.waiters, &pl.st);
                    pl.s.shutdown(ctx).await;
                }) as BoxFuture<'static, ()>
            }
            RuntimeState::Shutdown(fut) => fut,
            RuntimeState::Polling => return Poll::Pending,
            RuntimeState::Done => {
                self.st_runtime.replace(RuntimeState::Done);
                return Poll::Ready(());
            }
        };
        let result = fut.as_mut().poll(cx);
        if result.is_ready() {
            drop(fut);
            self.st_runtime.replace(RuntimeState::Done);
        } else {
            self.st_runtime.replace(RuntimeState::Shutdown(fut));
        }
        result
    }

    fn is_shutdown(&self) -> bool {
        self.waiters.is_shutdown()
    }
}

#[cfg(test)]
mod tests {
    use std::{future::pending, task::Waker};

    use super::*;
    use crate::Pipeline;

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
            let _g = Guard(&self.0);
            pending().await
        }

        async fn shutdown(&self, _: Ctx<'_, Self, usize>) {
            let _g = Guard(&self.0);
            pending::<()>().await;
        }
    }

    #[test]
    fn miri_drop_with_pending_readiness() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = Pipeline::new(1, Pending(vec![1, 2]));
        assert!(pl.poll_ready(&mut cx).is_pending());
        drop(pl);
    }

    #[test]
    fn miri_drop_with_pending_shutdown() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = Pipeline::new(1, Pending(vec![1, 2]));
        assert!(pl.poll_shutdown(&mut cx).is_pending());
        assert!(pl.is_shutdown());
        drop(pl);
    }

    #[test]
    fn miri_drop_pending_call_after_pipeline() {
        let mut cx = Context::from_waker(Waker::noop());
        let pl = Pipeline::new(1, Pending(vec![1, 2]));
        let mut call = Box::pin(pl.call_static(()));
        drop(pl);
        assert!(call.as_mut().poll(&mut cx).is_pending());
        drop(call);
    }
}
