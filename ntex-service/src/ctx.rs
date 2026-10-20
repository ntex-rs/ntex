use std::{
    cell, fmt, future::Future, marker, ops, pin, rc::Rc, task::Context, task::Poll, task::Waker,
};

use crate::readiness::{Generation, ReadyScope, RunCheck};
use crate::{ReadyNotifier, Service};

/// Context provided to [`Service`] lifecycle methods.
///
/// A context gives a service access to pipeline state and coordinates calls to
/// inner services with the pipeline's readiness and shutdown machinery.
/// It borrows the current readiness scope from a pipeline or service binding.
/// Copying or mapping the context preserves that scope; calling a binding
/// selects the binding's own scope without changing the caller's context.
pub struct Ctx<'a, Svc: ?Sized, St = ()> {
    idx: u32,
    st: &'a St,
    waiters: &'a WaitersRef,
    scope: &'a ReadyScope,
    call_nowait: bool,
    _t: marker::PhantomData<Rc<Svc>>,
}

#[derive(Debug)]
pub(crate) struct WaitersRef {
    scope: ReadyScope,
    cur: cell::Cell<u32>,
    running: cell::Cell<bool>,
    shutdown: cell::Cell<bool>,
    wakers: cell::UnsafeCell<Vec<u32>>,
    indexes: cell::UnsafeCell<slab::Slab<Option<Waker>>>,
}

impl WaitersRef {
    pub(crate) fn new() -> Self {
        let mut waiters = slab::Slab::with_capacity(16);
        waiters.insert(None);
        WaitersRef {
            scope: ReadyScope::new(),
            cur: cell::Cell::new(u32::MAX),
            running: cell::Cell::new(false),
            shutdown: cell::Cell::new(false),
            indexes: cell::UnsafeCell::new(waiters),
            wakers: cell::UnsafeCell::new(Vec::default()),
        }
    }

    #[allow(clippy::mut_from_ref)]
    pub(crate) fn get(&self) -> &mut slab::Slab<Option<Waker>> {
        unsafe { &mut *self.indexes.get() }
    }

    #[allow(clippy::mut_from_ref)]
    pub(crate) fn get_wakers(&self) -> &mut Vec<u32> {
        unsafe { &mut *self.wakers.get() }
    }

    pub(crate) fn insert(&self) -> u32 {
        self.get().insert(None) as u32
    }

    pub(crate) fn remove(&self, idx: u32) {
        self.get().remove(idx as usize);

        if self.cur.get() == idx {
            self.notify();
        }
    }

    pub(crate) fn notify(&self) {
        let indexes_to_wake = std::mem::take(self.get_wakers());
        let mut wakers = Vec::new();
        if !indexes_to_wake.is_empty() {
            let indexes = self.get();
            for idx in indexes_to_wake {
                if let Some(item) = indexes.get_mut(idx as usize)
                    && let Some(waker) = item.take()
                {
                    wakers.push(waker);
                }
            }
        }

        self.cur.set(u32::MAX);
        for waker in wakers {
            waker.wake();
        }
    }

    pub(crate) fn run<F, R>(&self, idx: u32, cx: &mut Context<'_>, f: F) -> Poll<R>
    where
        F: FnOnce(&mut Context<'_>) -> Poll<R>,
    {
        // Ctx::poll_xxx() methods requires current waker always available,
        // nested readiness checks poll with the same waker
        let slot = &mut self.get()[idx as usize];
        if !slot.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
            *slot = Some(cx.waker().clone());
        }

        // calculate owner for readiness check
        let cur = self.cur.get();
        let can_check = if cur == idx {
            true
        } else if cur == u32::MAX {
            self.cur.set(idx);
            true
        } else {
            false
        };

        if can_check {
            // only one readiness check can manage waiters
            let initial_run = !self.running.get();
            if initial_run {
                self.running.set(true);
            }

            let result = f(cx);

            if initial_run {
                if result.is_pending() {
                    self.get_wakers().push(idx);
                } else {
                    self.notify();
                }
                self.running.set(false);
            }
            result
        } else {
            // Another pipeline binding owns the readiness check.
            self.get_wakers().push(idx);
            Poll::Pending
        }
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.set(true);
    }

    pub(crate) fn scope(&self) -> &ReadyScope {
        &self.scope
    }

    pub(crate) fn register(&self) -> WaiterRegistration<'_> {
        WaiterRegistration {
            idx: self.insert(),
            waiters: self,
        }
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.get()
    }
}

pub(crate) struct WaiterRegistration<'a> {
    pub(crate) idx: u32,
    waiters: &'a WaitersRef,
}

impl Drop for WaiterRegistration<'_> {
    fn drop(&mut self) {
        self.waiters.remove(self.idx);
    }
}

impl<'a, Svc, St> Ctx<'a, Svc, St> {
    pub(crate) fn new(idx: u32, waiters: &'a WaitersRef, st: &'a St) -> Self {
        Self {
            idx,
            waiters,
            st,
            scope: &waiters.scope,
            call_nowait: false,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn scope(&self) -> &'a ReadyScope {
        self.scope
    }

    pub(crate) fn cast<S>(self) -> Ctx<'a, S, St> {
        self.with_scope(self.scope)
    }

    pub(crate) fn with_scope<S>(self, scope: &'a ReadyScope) -> Ctx<'a, S, St> {
        Ctx {
            idx: self.idx,
            st: self.st,
            waiters: self.waiters,
            scope,
            call_nowait: self.call_nowait,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn is_call_nowait(&self) -> bool {
        self.call_nowait
    }

    pub(crate) fn without_bypass(mut self) -> Self {
        self.call_nowait = false;
        self
    }

    #[inline]
    /// Returns the identifier of the current pipeline binding.
    pub fn id(&self) -> u32 {
        self.idx
    }

    #[inline]
    /// Returns the pipeline state.
    pub fn st(&'a self) -> &'a St {
        self.st
    }

    /// Subscribes the current readiness scope to a source of readiness changes.
    ///
    /// Subscribe before inspecting availability. Repeated subscriptions are
    /// idempotent and persist until the scope or source is dropped, including
    /// after a readiness check completes or is cancelled.
    pub fn subscribe(&self, source: &ReadyNotifier) {
        self.scope.subscribe(source);
    }

    /// Waits until `svc` is ready to process a request.
    ///
    /// A [`crate::ServiceBinding`] reuses successful readiness until a subscribed
    /// source changes. A raw service is checked on every invocation.
    pub fn ready<S, Req>(&self, svc: &'a S) -> impl Future<Output = Result<(), S::Error>>
    where
        S: Service<St, Req>,
    {
        self.ready_call(svc)
    }

    pub(crate) fn ready_generation<S, Req>(
        &self,
        svc: &'a S,
    ) -> impl Future<Output = Result<Generation, S::Error>>
    where
        S: Service<St, Req>,
    {
        self.ready_call(svc)
    }

    fn ready_call<S, Req, O>(&self, svc: &'a S) -> impl Future<Output = Result<O, S::Error>>
    where
        S: Service<St, Req>,
        O: From<Generation>,
    {
        let ctx = *self;
        ReadyCall {
            check: self.scope.check_uncached(move || svc.ready(ctx.cast())),
            waiters: self.waiters,
            idx: self.idx,
            active: false,
            _t: marker::PhantomData,
        }
    }

    #[inline]
    /// Waits for `svc` to become ready, then calls it.
    ///
    /// Uses the nearest upstream scope carried by this context. A valid cache
    /// allows immediate dispatch; otherwise, only `svc` is checked, without
    /// marking the enclosing scope ready. A [`crate::ServiceBinding`] also
    /// checks its own cache when called directly, independently of this scope.
    /// Calls do not consume cached readiness.
    pub async fn call<S, Req>(&self, svc: &'a S, req: Req) -> Result<S::Res, S::Error>
    where
        S: Service<St, Req>,
    {
        let ctx = self.without_bypass().cast();
        ctx.ready_cached(svc).await?;

        svc.call(req, ctx).await
    }

    pub(crate) async fn ready_cached<S, Req>(&self, svc: &'a S) -> Result<(), S::Error>
    where
        S: Service<St, Req>,
    {
        while !self.scope.is_ready() {
            let result = {
                let observer = self.scope.observer();
                let mut ready = pin::pin!(self.ready_generation(svc));
                std::future::poll_fn(|cx| {
                    observer.register(cx.waker());
                    // A queued caller can use success published by another checker.
                    if self.scope.is_ready() {
                        Poll::Ready(Ok(None))
                    } else {
                        ready.as_mut().poll(cx).map(|result| result.map(Some))
                    }
                })
                .await
            };
            let generation = result?;
            // Dropping the check or its observer can invalidate its success.
            if self.scope.is_ready()
                || generation.is_some_and(|generation| self.scope.is_current(generation))
            {
                return Ok(());
            }
        }
        Ok(())
    }

    #[inline]
    /// Calls `svc` without checking readiness.
    ///
    /// The caller must ensure that `svc` is ready. This also bypasses bindings
    /// behind transparent adapters, but nested calls through [`Ctx::call`]
    /// still perform their normal cache-aware readiness checks.
    pub async fn call_nowait<S, Req>(&self, svc: &'a S, req: Req) -> Result<S::Res, S::Error>
    where
        S: Service<St, Req>,
    {
        let mut ctx = self.cast();
        ctx.call_nowait = true;
        svc.call(req, ctx).await
    }

    #[inline]
    /// Polls a closure until completion using the dispatcher's task [`Context`].
    ///
    /// The [`Context`] uses the waker of the task that currently owns the readiness
    /// check. Outside of a readiness check a no-op waker is used, so a `Pending`
    /// result is not woken. If the current owner is not the main runner, the main
    /// runner is also scheduled for wake-up.
    pub async fn poll_fn<F, R>(&'a self, f: F) -> R
    where
        F: Fn(&mut Context<'a>) -> Poll<R>,
    {
        std::future::poll_fn(move |_| {
            let wakers = self.waiters.get();
            let idx = self.waiters.cur.get() as usize;
            let mut ctx = if let Some(w) = wakers.get(idx).and_then(|w| w.as_ref()) {
                Context::from_waker(w)
            } else {
                Context::from_waker(Waker::noop())
            };

            // if the current runner is not the main one, wake up the main task
            // so it re-registers all required wakers
            if idx != 0 {
                self.waiters.get_wakers().push(0);
            }

            f(&mut ctx)
        })
        .await
    }

    #[inline]
    /// Calls a closure once with the dispatcher's task [`Context`].
    ///
    /// The [`Context`] uses the waker of the task that currently owns the readiness
    /// check. Outside of a readiness check a no-op waker is used, so a `Pending`
    /// result is not woken. If the current owner is not the main runner, the main
    /// runner is also scheduled for wake-up.
    pub fn poll_once<F, R>(&'a self, f: F) -> R
    where
        F: FnOnce(&mut Context<'a>) -> R,
    {
        let wakers = self.waiters.get();
        let idx = self.waiters.cur.get() as usize;
        let mut ctx = if let Some(w) = wakers.get(idx).and_then(|w| w.as_ref()) {
            Context::from_waker(w)
        } else {
            Context::from_waker(Waker::noop())
        };

        // if the current runner is not the main one, wake up the main task
        // so it re-registers all required wakers
        if idx != 0 {
            self.waiters.get_wakers().push(0);
        }

        f(&mut ctx)
    }

    #[inline]
    /// Shuts down `svc`.
    pub async fn shutdown<S, Req>(&self, svc: &'a S)
    where
        S: Service<St, Req>,
    {
        svc.shutdown(self.cast()).await;
    }

    #[inline]
    /// Returns a context that uses `st` as its state.
    pub fn map_state<NewSt>(&'a self, st: &'a NewSt) -> Ctx<'a, Self, NewSt> {
        Ctx {
            st,
            idx: self.idx,
            waiters: self.waiters,
            scope: self.scope,
            call_nowait: self.call_nowait,
            _t: marker::PhantomData,
        }
    }
}

impl<S, St> Copy for Ctx<'_, S, St> {}

impl<S, St> Clone for Ctx<'_, S, St> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<S, St> ops::Deref for Ctx<'_, S, St> {
    type Target = St;

    #[inline]
    fn deref(&self) -> &St {
        self.st
    }
}

impl<S, St> fmt::Debug for Ctx<'_, S, St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("idx", &self.idx)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

// Hand-written to keep nested service readiness futures shallow, see `RunCheck`.
struct ReadyCall<'a, F, Fut, O> {
    check: RunCheck<'a, F, Fut>,
    waiters: &'a WaitersRef,
    idx: u32,
    // A started check that has not completed.
    active: bool,
    _t: marker::PhantomData<O>,
}

impl<F, Fut, O> Drop for ReadyCall<'_, F, Fut, O> {
    fn drop(&mut self) {
        if self.active && self.waiters.cur.get() == self.idx {
            self.waiters.notify();
        }
    }
}

impl<F, Fut, E, O> Future for ReadyCall<'_, F, Fut, O>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), E>>,
    O: From<Generation>,
{
    type Output = Result<O, E>;

    fn poll(self: pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `check` is pinned in place.
        let this = unsafe { self.get_unchecked_mut() };
        loop {
            this.active = true;
            let mut check = unsafe { pin::Pin::new_unchecked(&mut this.check) };
            let result = this
                .waiters
                .run(this.idx, cx, |cx| check.as_mut().poll_check(cx));
            let Poll::Ready(result) = result else {
                return Poll::Pending;
            };
            this.active = false;
            match result {
                // Scheduling another waiter can invalidate the completed check.
                Ok(generation) if !this.check.scope().is_current(generation) => {}
                result => return Poll::Ready(result.map(O::from)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, future::poll_fn};

    use ntex::channel::{condition, oneshot};
    use ntex::{rt::spawn, time, util::lazy, util::select};

    use super::*;
    use crate::Pipeline;

    struct Srv(Rc<Cell<usize>>, condition::Waiter, ReadyNotifier);

    impl Service<(), &'static str> for Srv {
        type Res = &'static str;
        type Error = ();

        async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
            ctx.subscribe(&self.2);
            self.0.set(self.0.get() + 1);
            self.1.ready().await;
            Ok(())
        }

        async fn call(
            &self,
            req: &'static str,
            ctx: Ctx<'_, Self>,
        ) -> Result<Self::Res, Self::Error> {
            let _ = format!("{ctx:?}");
            let _ = format!("{:?}", ctx.id());
            let () = *ctx;
            #[allow(clippy::clone_on_copy)]
            let _ = ctx.clone();
            Ok(req)
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let changed = ReadyNotifier::new();
        let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), changed.clone()));
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        con.notify(());
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 1);

        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 1);
        changed.notify();
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 2);

        con.notify(());
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 2);

        changed.notify();
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 3);
    }

    struct WakeCounter {
        clones: Cell<usize>,
        wakes: Cell<usize>,
    }

    fn counting_waker() -> (&'static WakeCounter, Waker) {
        use std::task::{RawWaker, RawWakerVTable};

        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);

        unsafe fn clone(data: *const ()) -> RawWaker {
            let cnt = unsafe { &*data.cast::<WakeCounter>() };
            cnt.clones.set(cnt.clones.get() + 1);
            RawWaker::new(data, &VTABLE)
        }
        unsafe fn wake(data: *const ()) {
            let cnt = unsafe { &*data.cast::<WakeCounter>() };
            cnt.wakes.set(cnt.wakes.get() + 1);
        }
        unsafe fn drop(_: *const ()) {}

        let cnt: &'static WakeCounter = Box::leak(Box::new(WakeCounter {
            clones: Cell::new(0),
            wakes: Cell::new(0),
        }));
        let raw = RawWaker::new(std::ptr::from_ref(cnt).cast(), &VTABLE);
        (cnt, unsafe { Waker::from_raw(raw) })
    }

    /// Wakes the dispatcher's task context during the readiness check
    struct WakeSrv;

    impl Service<(), &'static str> for WakeSrv {
        type Res = &'static str;
        type Error = ();

        async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
            ctx.poll_once(|cx| cx.waker().wake_by_ref());
            Ok(())
        }

        async fn call(&self, req: &'static str, _: Ctx<'_, Self>) -> Result<&'static str, ()> {
            Ok(req)
        }
    }

    struct Nested<S>(S);

    impl<S: Service<(), &'static str>> Service<(), &'static str> for Nested<S> {
        type Res = S::Res;
        type Error = S::Error;

        async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
            ctx.ready(&self.0).await
        }

        async fn call(
            &self,
            req: &'static str,
            ctx: Ctx<'_, Self>,
        ) -> Result<Self::Res, Self::Error> {
            ctx.call(&self.0, req).await
        }
    }

    #[ntex::test]
    async fn test_ready_waker_not_recloned() {
        let srv = Pipeline::new((), Nested(Nested(WakeSrv)));

        let (cnt1, waker1) = counting_waker();
        let mut cx = Context::from_waker(&waker1);
        assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));
        let clones = cnt1.clones.get();
        let wakes = cnt1.wakes.get();
        for _ in 0..4 {
            assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));
        }
        assert_eq!(cnt1.clones.get(), clones);
        assert_eq!(cnt1.wakes.get(), wakes);

        // a different waker replaces the stored one
        let (cnt2, waker2) = counting_waker();
        let mut cx = Context::from_waker(&waker2);
        assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));
        assert_eq!(cnt2.clones.get(), 1);
        assert_eq!(cnt2.wakes.get(), 0);
        assert_eq!(cnt1.wakes.get(), wakes);
    }

    #[ntex::test]
    async fn test_ready_on_drop() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));

        let srv1 = srv.bind();
        let (tx, rx) = oneshot::channel();
        spawn(async move {
            select(rx, srv1.ready()).await;
            time::sleep(time::Millis(25000)).await;
        });
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        let _ = tx.send(());
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        con.notify(());
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_ready_after_shutdown() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        let (tx, rx) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        spawn(async move {
            select(rx, srv.ready()).await;
            srv.shutdown().await;
            let _ = tx2.send(srv);
        });
        time::sleep(time::Millis(250)).await;

        let _ = tx.send(());
        let srv = rx2.await.unwrap();

        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        con.notify(());
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_pipeline_binding_after_shutdown() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));
        poll_fn(|cx| srv.poll_shutdown(cx)).await;
        let _ = poll_fn(|cx| srv.poll_ready(cx)).await;
    }

    #[ntex::test]
    async fn test_shared_call() {
        let data = Rc::new(RefCell::new(Vec::new()));

        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));

        let srv1 = srv.bind();
        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let _ = srv1.ready().await;
            let fut = srv1.call_static("srv1");
            assert!(format!("{fut:?}").contains("PipelineCall"));
            let i = fut.await.unwrap();
            data1.borrow_mut().push(i);
        });

        let srv2 = srv.bind();
        let data2 = data.clone();
        ntex::rt::spawn(async move {
            let i = srv2.call("srv2").await.unwrap();
            data2.borrow_mut().push(i);
        });
        time::sleep(time::Millis(50)).await;

        con.notify(());
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 1);
        assert_eq!(&*data.borrow(), &["srv1", "srv2"]);
    }
}
