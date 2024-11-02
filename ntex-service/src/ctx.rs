use std::task::{Context, Poll, Waker};
use std::{cell, fmt, future::Future, marker, pin::Pin, rc::Rc};

use crate::Service;

pub struct ServiceCtx<'a, S: ?Sized> {
    idx: u32,
    waiters: &'a WaitersRef,
    _t: marker::PhantomData<Rc<S>>,
}

pub(crate) struct WaitersRef {
    pub(crate) cur_1: cell::Cell<u32>,
    pub(crate) cur_2: cell::Cell<u32>,
    pub(crate) indexes: cell::UnsafeCell<slab::Slab<Item>>,
}

#[derive(Debug, Default)]
pub(crate) struct Item {
    ready: Option<Waker>,
    not_ready: Option<Waker>,
}

impl WaitersRef {
    pub(crate) fn new() -> (u32, Self) {
        let mut waiters = slab::Slab::new();
        let index = waiters.insert(Default::default()) as u32;

        (
            index,
            WaitersRef {
                cur_1: cell::Cell::new(u32::MAX),
                cur_2: cell::Cell::new(u32::MAX),
                indexes: cell::UnsafeCell::new(waiters),
            },
        )
    }

    #[allow(clippy::mut_from_ref)]
    pub(crate) fn get(&self) -> &mut slab::Slab<Item> {
        unsafe { &mut *self.indexes.get() }
    }

    pub(crate) fn insert(&self) -> u32 {
        self.get().insert(Default::default()) as u32
    }

    pub(crate) fn remove(&self, idx: u32) {
        self.notify();
        self.get().remove(idx as usize);
    }

    pub(crate) fn remove_index(&self, idx: u32) {
        self.get().remove(idx as usize);

        if self.cur_1.get() == idx || self.cur_2.get() == idx {
            self.notify();
        }
    }

    pub(crate) fn register(&self, idx: u32, st: ServiceState, cx: &mut Context<'_>) {
        match st {
            ServiceState::Ready => {
                self.get()[idx as usize].ready = Some(cx.waker().clone())
            }
            ServiceState::NotReady => {
                self.get()[idx as usize].not_ready = Some(cx.waker().clone())
            }
        }
    }

    pub(crate) fn notify(&self) {
        for (_, item) in self.get().iter_mut() {
            if let Some(waker) = item.ready.take() {
                waker.wake();
            }
            if let Some(waker) = item.not_ready.take() {
                waker.wake();
            }
        }

        self.cur_1.set(u32::MAX);
        self.cur_2.set(u32::MAX);
    }

    pub(crate) fn len(&self) -> usize {
        self.get().len()
    }

    fn get_cur(&self, st: ServiceState) -> u32 {
        match st {
            ServiceState::Ready => self.cur_1.get(),
            ServiceState::NotReady => self.cur_2.get(),
        }
    }

    fn set_cur(&self, st: ServiceState, idx: u32) {
        match st {
            ServiceState::Ready => self.cur_1.set(idx),
            ServiceState::NotReady => self.cur_2.set(idx),
        }
    }

    pub(crate) fn can_check(
        &self,
        idx: u32,
        st: ServiceState,
        cx: &mut Context<'_>,
    ) -> bool {
        let cur = self.get_cur(st);
        if cur == idx {
            true
        } else if cur == u32::MAX {
            self.set_cur(st, idx);
            true
        } else {
            self.register(idx, st, cx);
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
            st: ServiceState::Ready,
            completed: false,
            fut: svc.state(
                ServiceState::Ready,
                ServiceCtx {
                    idx: self.idx,
                    waiters: self.waiters,
                    _t: marker::PhantomData,
                },
            ),
            ctx: *self,
        }
        .await
    }

    /// Returns when the service changes internal state according parameter
    ///
    /// Note. `NotReady` does not provide shared readiness
    pub async fn state<T, R>(&self, svc: &'a T, st: ServiceState) -> Result<(), T::Error>
    where
        T: Service<R>,
    {
        match st {
            ServiceState::Ready => self.ready(svc).await,
            ServiceState::NotReady => {
                svc.state(
                    ServiceState::NotReady,
                    ServiceCtx {
                        idx: self.idx,
                        waiters: self.waiters,
                        _t: marker::PhantomData,
                    },
                )
                .await
            }
        }
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
    st: ServiceState,
    completed: bool,
    fut: F,
    ctx: ServiceCtx<'a, S>,
}

impl<'a, S: ?Sized, F: Future> Drop for ReadyCall<'a, S, F> {
    fn drop(&mut self) {
        if !self.completed && self.ctx.waiters.get_cur(self.st) == self.ctx.idx {
            self.ctx.waiters.notify();
        }
    }
}

impl<'a, S: ?Sized, F: Future> Unpin for ReadyCall<'a, S, F> {}

impl<'a, S: ?Sized, F: Future> Future for ReadyCall<'a, S, F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.ctx.waiters.can_check(self.ctx.idx, self.st, cx) {
            // SAFETY: `fut` never moves
            let result = unsafe { Pin::new_unchecked(&mut self.as_mut().fut).poll(cx) };
            match result {
                Poll::Pending => {
                    self.ctx.waiters.register(self.ctx.idx, self.st, cx);
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
    use std::{cell::Cell, cell::RefCell, future::poll_fn, task::Poll};

    use ntex_util::channel::{condition, oneshot};
    use ntex_util::{future::lazy, future::select, spawn, time};

    use super::*;
    use crate::{Pipeline, ServiceState};

    struct Srv(Rc<Cell<usize>>, condition::Waiter);

    impl Service<&'static str> for Srv {
        type Response = &'static str;
        type Error = ();

        async fn state(&self, _: ServiceState, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
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
