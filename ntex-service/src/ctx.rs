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
    cur: cell::Cell<u32>,
    wakers: cell::UnsafeCell<Vec<u32>>,
    indexes: cell::UnsafeCell<slab::Slab<Option<Waker>>>,
}

impl WaitersRef {
    pub(crate) fn new() -> (u32, Self) {
        let mut waiters = slab::Slab::new();

        // first insert for wake ups from services
        let _ = waiters.insert(None);

        (
            waiters.insert(Default::default()) as u32,
            WaitersRef {
                cur: cell::Cell::new(u32::MAX),
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

    pub(crate) fn register(&self, idx: u32, cx: &mut Context<'_>) {
        self.get()[idx as usize] = Some(cx.waker().clone());
        self.get_wakers().push(idx);
    }

    pub(crate) fn register_unready(&self, cx: &mut Context<'_>) {
        self.get()[0] = Some(cx.waker().clone());
        self.get_wakers().push(0);
    }

    pub(crate) fn notify(&self) {
        let indexes = self.get();
        let wakers = self.get_wakers();

        for idx in wakers.drain(..) {
            if let Some(item) = indexes.get_mut(idx as usize) {
                if let Some(waker) = item.take() {
                    waker.wake();
                }
            }
        }

        self.cur.set(u32::MAX);
    }

    pub(crate) fn can_check(&self, idx: u32, cx: &mut Context<'_>) -> bool {
        let cur = self.cur.get();
        if cur == idx {
            true
        } else if cur == u32::MAX {
            self.cur.set(idx);
            true
        } else {
            self.register(idx, cx);
            false
        }
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

impl<'a, S> Copy for ServiceCtx<'a, S> {}

impl<'a, S> Clone for ServiceCtx<'a, S> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, S> fmt::Debug for ServiceCtx<'a, S> {
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

impl<'a, S: ?Sized, F: Future> Drop for ReadyCall<'a, S, F> {
    fn drop(&mut self) {
        if !self.completed && self.ctx.waiters.cur.get() == self.ctx.idx {
            self.ctx.waiters.notify();
        }
    }
}

impl<'a, S: ?Sized, F: Future> Unpin for ReadyCall<'a, S, F> {}

impl<'a, S: ?Sized, F: Future> Future for ReadyCall<'a, S, F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.ctx.waiters.can_check(self.ctx.idx, cx) {
            // SAFETY: `fut` never moves
            let result = unsafe { Pin::new_unchecked(&mut self.as_mut().fut).poll(cx) };
            match result {
                Poll::Pending => {
                    self.ctx.waiters.register(self.ctx.idx, cx);
                    Poll::Pending
                }
                Poll::Ready(res) => {
                    self.completed = true;
                    self.ctx.waiters.notify();
                    Poll::Ready(res)
                }
            }
        } else {
            Poll::Pending
        }
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
            let _ = format!("{:?}", ctx);
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
            drop(srv1);
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
            drop(srv1);
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
    async fn test_shared_call() {
        let data = Rc::new(RefCell::new(Vec::new()));

        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait())).bind();
        let srv2 = srv1.clone();

        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let _ = poll_fn(|cx| srv1.poll_ready(cx)).await;
            let i = srv1.call_nowait("srv1").await.unwrap();
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
