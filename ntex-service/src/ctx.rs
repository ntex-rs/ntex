use std::task::{Context, Poll, Waker};
use std::{cell, fmt, future::Future, marker, pin::Pin, rc::Rc};

use crate::Service;

pub struct ServiceCtx<'a, S: ?Sized> {
    idx: u32,
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
            waiters.insert(Default::default()) as u32,
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
                if let Some(item) = indexes.get_mut(idx as usize) {
                    if let Some(waker) = item.take() {
                        waker.wake();
                    }
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

impl<'a, S> ServiceCtx<'a, S> {
    pub(crate) fn new(idx: u32, waiters: &'a WaitersRef) -> Self {
        Self {
            idx,
            waiters,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn inner(self) -> (u32, &'a WaitersRef) {
        (self.idx, self.waiters)
    }

    #[inline]
    /// Unique id for this pipeline
    pub fn id(&self) -> u32 {
        self.idx
    }

    /// Returns when the service is able to process requests.
    pub async fn ready<T, R>(&self, svc: &'a T) -> Result<(), T::Error>
    where
        T: Service<R>,
    {
        // check readiness and notify waiters
        ReadyCall {
            completed: false,
            fut: svc.ready(ServiceCtx {
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            }),
            ctx: *self,
        }
        .await
    }

    #[inline]
    /// Wait for service readiness and then call service
    pub async fn call<T, R>(&self, svc: &'a T, req: R) -> Result<T::Response, T::Error>
    where
        T: Service<R>,
        R: 'a,
    {
        self.ready(svc).await?;

        svc.call(
            req,
            ServiceCtx {
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            },
        )
        .await
    }

    #[inline]
    /// Call service, do not check service readiness
    pub async fn call_nowait<T, R>(
        &self,
        svc: &'a T,
        req: R,
    ) -> Result<T::Response, T::Error>
    where
        T: Service<R>,
        R: 'a,
    {
        svc.call(
            req,
            ServiceCtx {
                idx: self.idx,
                waiters: self.waiters,
                _t: marker::PhantomData,
            },
        )
        .await
    }
}

impl<S> Copy for ServiceCtx<'_, S> {}

impl<S> Clone for ServiceCtx<'_, S> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<S> fmt::Debug for ServiceCtx<'_, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceCtx")
            .field("idx", &self.idx)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

struct ReadyCall<'a, S: ?Sized, F: Future> {
    completed: bool,
    fut: F,
    ctx: ServiceCtx<'a, S>,
}

impl<S: ?Sized, F: Future> Drop for ReadyCall<'_, S, F> {
    fn drop(&mut self) {
        if !self.completed && self.ctx.waiters.cur.get() == self.ctx.idx {
            self.ctx.waiters.notify();
        }
    }
}

impl<S: ?Sized, F: Future> Unpin for ReadyCall<'_, S, F> {}

impl<S: ?Sized, F: Future> Future for ReadyCall<'_, S, F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.ctx.waiters.run(self.ctx.idx, cx, |cx| {
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
mod tests {
    use std::{cell::Cell, cell::RefCell, future::poll_fn};

    use ntex_util::channel::{condition, oneshot};
    use ntex_util::{future::lazy, future::select, spawn, time};

    use super::*;
    use crate::Pipeline;

    struct Srv(Rc<Cell<usize>>, condition::Waiter);

    impl Service<&'static str> for Srv {
        type Response = &'static str;
        type Error = ();

        async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            self.1.ready().await;
            Ok(())
        }

        async fn call(
            &self,
            req: &'static str,
            ctx: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, Self::Error> {
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

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait())).bind();
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
        let srv2 = srv1.clone().bind();

        let (tx, rx) = oneshot::channel();
        spawn(async move {
            select(rx, srv1.ready()).await;
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

        let srv1 = srv.clone().bind();
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
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait())).bind();
        let _ = poll_fn(|cx| srv.poll_shutdown(cx)).await;
        let _ = poll_fn(|cx| srv.poll_ready(cx)).await;
    }

    #[ntex::test]
    async fn test_shared_call() {
        let data = Rc::new(RefCell::new(Vec::new()));

        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait())).bind();
        let srv2 = srv1.clone();
        let _: Pipeline<_> = srv1.pipeline();

        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let _ = poll_fn(|cx| srv1.poll_ready(cx)).await;
            let fut = srv1.call_nowait("srv1");
            assert!(format!("{:?}", fut).contains("PipelineCall"));
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
