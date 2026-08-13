#![allow(clippy::cast_possible_truncation)]
use std::task::{Context, Poll, Waker};
use std::{cell, fmt, future::Future, marker, pin::Pin, ptr, rc::Rc};

use crate::Service;

pub struct Ctx<'a, S: Service + ?Sized, St: ?Sized> {
    idx: u32,
    st: &'a St,
    waiters: &'a WaitersRef,
    _t: marker::PhantomData<Rc<S>>,
}

pub struct ReadyCtx<'a, S: Service + ?Sized, St: ?Sized> {
    idx: u32,
    st: Option<&'a St>,
    waiters: &'a WaitersRef,
    _t: marker::PhantomData<Rc<S>>,
}

#[derive(Debug)]
pub(crate) struct WaitersRef {
    running: cell::Cell<bool>,
    cur: cell::Cell<u32>,
    shutdown: cell::Cell<bool>,
    wakers: cell::UnsafeCell<Vec<u32>>,
    indexes: cell::UnsafeCell<slab::Slab<Option<Waker>>>,
}

impl WaitersRef {
    pub(crate) fn new() -> (u32, Self) {
        let mut waiters = slab::Slab::new();

        (
            waiters.insert(None) as u32,
            WaitersRef {
                running: cell::Cell::new(false),
                cur: cell::Cell::new(u32::MAX),
                shutdown: cell::Cell::new(false),
                indexes: cell::UnsafeCell::new(waiters),
                wakers: cell::UnsafeCell::new(Vec::default()),
            },
        )
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
        let wakers = self.get_wakers();
        if !wakers.is_empty() {
            let indexes = self.get();
            for idx in wakers.drain(..) {
                if let Some(item) = indexes.get_mut(idx as usize)
                    && let Some(waker) = item.take()
                {
                    waker.wake();
                }
            }
        }

        self.cur.set(u32::MAX);
    }

    pub(crate) fn run<F, R>(&self, idx: u32, cx: &mut Context<'_>, f: F) -> Poll<R>
    where
        F: FnOnce(&mut Context<'_>) -> Poll<R>,
    {
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
                    self.get()[idx as usize] = Some(cx.waker().clone());
                } else {
                    self.notify();
                }
                self.running.set(false);
            }
            result
        } else {
            // other pipeline ownes readiness check process
            self.get_wakers().push(idx);
            self.get()[idx as usize] = Some(cx.waker().clone());
            Poll::Pending
        }
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.set(true);
    }

    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown.get()
    }
}

impl<'a, S: Service, St> Ctx<'a, S, St> {
    pub(crate) fn new(idx: u32, waiters: &'a WaitersRef, st: &'a St) -> Self {
        Self {
            idx,
            waiters,
            st,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn inner(self) -> (u32, &'a WaitersRef, &'a St) {
        (self.idx, self.waiters, self.st)
    }

    #[inline]
    /// Unique id for this pipeline
    pub fn id(&self) -> u32 {
        self.idx
    }

    #[inline]
    /// Application state
    pub fn st(&'a self) -> &'a S::St {
        self.st
    }

    /// Returns when the service is able to process requests.
    pub async fn ready<T>(&self, svc: &'a T) -> Result<(), T::Error>
    where
        T: Service<St>,
    {
        // check readiness and notify waiters
        ReadyCall {
            completed: false,
            fut: svc.ready(ReadyCtx {
                st: Some(self.st),
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            }),
            idx: self.idx,
            waiters: self.waiters,
        }
        .await
    }

    /// Returns when the service is able to process requests.
    pub(crate) async fn ready_with_st<T, St1>(
        &self,
        svc: &'a T,
        st: &'a St1,
    ) -> Result<(), T::Error>
    where
        T: Service<St1>,
    {
        // check readiness and notify waiters
        ReadyCall {
            completed: false,
            fut: svc.ready(ReadyCtx {
                st: Some(st),
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            }),
            idx: self.idx,
            waiters: self.waiters,
        }
        .await
    }

    #[inline]
    /// Wait for service readiness and then call service
    pub async fn call<T>(&self, svc: &'a T, req: T::Req) -> Result<T::Res, T::Error>
    where
        T: Service<St>,
    {
        self.ready(svc).await?;

        svc.call(
            req,
            Ctx {
                idx: self.idx,
                st: self.st,
                waiters: self.waiters,
                _t: marker::PhantomData,
            },
        )
        .await
    }

    #[inline]
    /// Wait for service readiness and then call service
    pub(crate) async fn call_with_st<T, St1>(
        &self,
        svc: &'a T,
        req: T::Req,
        st: &St1,
    ) -> Result<T::Res, T::Error>
    where
        T: Service<St1>,
    {
        self.ready_with_st(svc, st).await?;

        svc.call(
            req,
            Ctx {
                st,
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            },
        )
        .await
    }

    #[inline]
    /// Call service, do not check service readiness
    pub async fn call_nowait<T>(&self, svc: &'a T, req: T::Req) -> Result<T::Res, T::Error>
    where
        T: Service<St>,
    {
        svc.call(
            req,
            Ctx {
                st: self.st,
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            },
        )
        .await
    }
}

impl<S: Service, St> Copy for Ctx<'_, S, St> {}

impl<S: Service, St> Clone for Ctx<'_, S, Sta> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: Service, St> fmt::Debug for Ctx<'_, S, St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ctx")
            .field("idx", &self.idx)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

impl<'a, S: Service, St> ReadyCtx<'a, S, St> {
    pub(crate) fn new(idx: u32, waiters: &'a WaitersRef, st: Option<&'a St>) -> Self {
        Self {
            idx,
            waiters,
            st,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn inner(self) -> (u32, &'a WaitersRef, Option<&'a St>) {
        (self.idx, self.waiters, self.st)
    }

    #[inline]
    /// Unique id for this pipeline
    pub fn id(&self) -> u32 {
        self.idx
    }

    #[inline]
    /// Application state
    pub fn st(&'a self) -> Option<&'a St> {
        self.st
    }

    /// Returns when the service is able to process requests.
    pub async fn ready<T>(&self, svc: &'a T) -> Result<(), T::Error>
    where
        T: Service<St>,
    {
        // check readiness and notify waiters
        ReadyCall {
            completed: false,
            fut: svc.ready(ReadyCtx {
                st: self.st,
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            }),
            idx: self.idx,
            waiters: self.waiters,
        }
        .await
    }

    /// Returns when the service is able to process requests.
    pub(crate) async fn ready_with_st<T, St1>(
        &self,
        svc: &'a T,
        st: &'a St1,
    ) -> Result<(), T::Error>
    where
        T: Service<St1>,
    {
        // check readiness and notify waiters
        ReadyCall {
            completed: false,
            fut: svc.ready(ReadyCtx {
                st: Some(st),
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            }),
            idx: self.idx,
            waiters: self.waiters,
        }
        .await
    }
}

impl<S: Service, St> Copy for ReadyCtx<'_, S, St> {}

impl<S: Service, St> Clone for ReadyCtx<'_, S, St> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<S: Service, St> fmt::Debug for ReadyCtx<'_, S, St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyCtx")
            .field("idx", &self.idx)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

struct ReadyCall<'a, F: Future> {
    completed: bool,
    fut: F,
    idx: u32,
    waiters: &'a WaitersRef,
}

impl<F: Future> Drop for ReadyCall<'_, F> {
    fn drop(&mut self) {
        if !self.completed && self.waiters.cur.get() == self.idx {
            self.waiters.notify();
        }
    }
}

impl<F: Future> Unpin for ReadyCall<'_, F> {}

impl<F: Future> Future for ReadyCall<'_, F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waiters.run(self.idx, cx, |cx| {
            // SAFETY: `fut` never moves
            let result = unsafe { Pin::new_unchecked(&mut self.as_mut().fut).poll(cx) };
            if result.is_ready() {
                self.completed = true;
            }
            result
        })
    }
}

#[cfg(test)]
#[allow(clippy::should_panic_without_expect, clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, cell::RefCell, future::poll_fn};

    use ntex::channel::{condition, oneshot};
    use ntex::{rt::spawn, time, util::lazy, util::select};

    use super::*;
    use crate::Pipeline;

    struct Srv(Rc<Cell<usize>>, condition::Waiter);

    impl Service for Srv {
        type St = ();
        type Req = &'static str;
        type Res = &'static str;
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            self.1.ready().await;
            Ok(())
        }

        async fn call(
            &self,
            req: &'static str,
            ctx: ServiceCtx<'_, Self>,
        ) -> Result<Self::Res, Self::Error> {
            let _ = format!("{ctx:?}");
            let _ = format!("{:?}", ctx.id());
            #[allow(clippy::clone_on_copy)]
            let _ = ctx.clone();
            Ok(req)
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait())).bind(());
        let srv2 = srv1.clone();

        let res = lazy(|cx| srv1.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        con.notify();
        let res = lazy(|cx| srv1.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 1);

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 2);

        con.notify();
        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 2);

        let res = lazy(|cx| srv1.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 3);
    }

    #[ntex::test]
    async fn test_ready_on_drop() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait()));

        let srv1 = srv.clone();
        let srv2 = srv1.clone().bind(());

        let (tx, rx) = oneshot::channel();
        spawn(async move {
            select(rx, srv1.ready(&())).await;
            time::sleep(time::Millis(25000)).await;
        });
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        let _ = tx.send(());
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        con.notify();
        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_ready_after_shutdown() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait()));

        let srv1 = srv.clone().bind(());
        let srv2 = srv1.clone();

        let (tx, rx) = oneshot::channel();
        spawn(async move {
            select(rx, poll_fn(|cx| srv1.poll_ready(cx))).await;
            poll_fn(|cx| srv1.poll_shutdown(cx)).await;
            time::sleep(time::Millis(25000)).await;
        });
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        let _ = tx.send(());
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        con.notify();
        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    #[should_panic]
    async fn test_pipeline_binding_after_shutdown() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait())).bind(());
        poll_fn(|cx| srv.poll_shutdown(cx)).await;
        let _ = poll_fn(|cx| srv.poll_ready(cx)).await;
    }

    #[ntex::test]
    async fn test_shared_call() {
        let data = Rc::new(RefCell::new(Vec::new()));

        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait())).bind(());
        let srv2 = srv1.clone();
        let _: Pipeline<_> = srv1.pipeline();

        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let _ = poll_fn(|cx| srv1.poll_ready(cx)).await;
            let fut = srv1.call_nowait("srv1");
            assert!(format!("{fut:?}").contains("PipelineCall"));
            let i = fut.await.unwrap();
            data1.borrow_mut().push(i);
        });

        let data2 = data.clone();
        ntex::rt::spawn(async move {
            let i = srv2.call("srv2").await.unwrap();
            data2.borrow_mut().push(i);
        });
        time::sleep(time::Millis(50)).await;

        con.notify();
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 2);
        assert_eq!(&*data.borrow(), &["srv1"]);

        con.notify();
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 2);
        assert_eq!(&*data.borrow(), &["srv1", "srv2"]);
    }
}
