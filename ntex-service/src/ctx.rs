use std::task::{Context, Poll, Waker};
use std::{cell, fmt, future::Future, marker, pin::Pin, rc::Rc};

use crate::Service;

pub struct ServiceCtx<'a, S: ?Sized> {
    idx: u32,
    waiters: &'a WaitersRef,
    _t: marker::PhantomData<Rc<S>>,
}

pub(crate) struct Waiters {
    index: u32,
    pub waiters: Rc<WaitersRef>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
    struct Flags: u8 {
        const BOUND      = 0b0000_0001;
        const READY      = 0b0000_0010;
    }
}

pub(crate) struct WaitersRef {
    cur: cell::Cell<u32>,
    state: cell::Cell<Flags>,
    indexes: cell::UnsafeCell<slab::Slab<Option<Waker>>>,
}

impl WaitersRef {
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut slab::Slab<Option<Waker>> {
        unsafe { &mut *self.indexes.get() }
    }

    fn insert_flag(&self, fl: Flags) {
        let mut flags = self.state.get();
        flags.insert(fl);
        self.state.set(flags);
    }

    fn remove_flag(&self, fl: Flags) {
        let mut flags = self.state.get();
        flags.remove(fl);
        self.state.set(flags);
    }

    fn insert(&self) -> u32 {
        self.get().insert(None) as u32
    }

    fn remove(&self, idx: u32) {
        self.notify();
        self.get().remove(idx as usize);
    }

    fn register(&self, idx: u32, cx: &mut Context<'_>) {
        self.get()[idx as usize] = Some(cx.waker().clone());
    }

    fn notify(&self) {
        for (_, waker) in self.get().iter_mut() {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }

        self.cur.set(u32::MAX);
    }

    pub(crate) fn len(&self) -> usize {
        self.get().len()
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.state.get().contains(Flags::READY)
    }

    pub(crate) fn is_bound(&self) -> bool {
        self.state.get().contains(Flags::BOUND)
    }

    pub(crate) fn bind(&self) {
        self.state.set(Flags::BOUND);
        self.notify();
    }

    pub(crate) fn unbind(&self) {
        self.state.set(Flags::empty());
        self.notify();
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

impl Waiters {
    pub(crate) fn new() -> Self {
        let mut waiters = slab::Slab::new();
        let index = waiters.insert(None) as u32;
        Waiters {
            index,
            waiters: Rc::new(WaitersRef {
                cur: cell::Cell::new(u32::MAX),
                state: cell::Cell::new(Flags::empty()),
                indexes: cell::UnsafeCell::new(waiters),
            }),
        }
    }

    pub(crate) fn get_ref(&self) -> &WaitersRef {
        self.waiters.as_ref()
    }

    pub(crate) fn is_bound(&self) -> bool {
        self.waiters.state.get().contains(Flags::BOUND)
    }

    pub(crate) fn set_ready(&self) {
        if !self.waiters.state.get().contains(Flags::READY) {
            self.waiters.insert_flag(Flags::READY);
            self.waiters.notify();
        }
    }

    pub(crate) fn set_notready(&self) {
        self.waiters.remove_flag(Flags::READY);
    }
}

impl Drop for Waiters {
    #[inline]
    fn drop(&mut self) {
        self.waiters.remove(self.index);
        if self.waiters.cur.get() == self.index {
            self.waiters.notify();
        }
    }
}

impl Clone for Waiters {
    fn clone(&self) -> Self {
        Waiters {
            index: self.waiters.insert(),
            waiters: self.waiters.clone(),
        }
    }
}

impl fmt::Debug for Waiters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waiters")
            .field("index", &self.index)
            .field("waiters", &self.waiters.get().len())
            .finish()
    }
}

impl<'a, S> ServiceCtx<'a, S> {
    pub(crate) fn new(waiters: &'a Waiters) -> Self {
        Self {
            idx: waiters.index,
            waiters: waiters.get_ref(),
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn from_ref(idx: u32, waiters: &'a WaitersRef) -> Self {
        Self {
            idx,
            waiters,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn inner(self) -> (u32, &'a WaitersRef) {
        (self.idx, self.waiters)
    }

    /// Mark service as un-ready, force readiness re-evaluation for pipeline
    pub fn unready(&self) {
        if self.waiters.state.get().contains(Flags::READY) {
            self.waiters.remove_flag(Flags::READY);
            self.waiters.notify();
        }
    }

    /// Returns when the service is able to process requests.
    pub async fn check_readiness<T, R>(&self, svc: &'a T) -> Result<(), T::Error>
    where
        T: Service<R>,
    {
        // active readiness
        if self.waiters.is_ready() {
            Ok(())
        } else {
            // check readiness and notify waiters
            ReadyCall {
                fut: Some(svc.ready()),
                fut1: None,
                ctx: *self,
                completed: false,
                _t: marker::PhantomData,
            }
            .await
        }
    }

    #[inline]
    /// Wait for service readiness and then call service
    pub async fn call<T, R>(&self, svc: &'a T, req: R) -> Result<T::Response, T::Error>
    where
        T: Service<R>,
        R: 'a,
    {
        self.check_readiness(svc).await?;

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

struct ReadyCall<'a, S: ?Sized, F: Future<Output = Option<R>>, E, R> {
    completed: bool,
    fut: Option<F>,
    fut1: Option<R>,
    ctx: ServiceCtx<'a, S>,
    _t: marker::PhantomData<(R, E)>,
}

impl<'a, S: ?Sized, F: Future<Output = Option<R>>, E, R> Drop
    for ReadyCall<'a, S, F, E, R>
{
    fn drop(&mut self) {
        if !self.completed && !self.ctx.waiters.is_bound() {
            self.ctx.waiters.notify();
        }
    }
}

impl<'a, S: ?Sized, F: Future<Output = Option<R>>, E, R> Unpin
    for ReadyCall<'a, S, F, E, R>
{
}

impl<'a, S: ?Sized, F: Future<Output = Option<R>>, E, R> Future
    for ReadyCall<'a, S, F, E, R>
where
    R: Future<Output = Result<(), E>>,
{
    type Output = Result<(), E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let st = self.ctx.waiters.state.get();
        if st.contains(Flags::BOUND) {
            if st.contains(Flags::READY) {
                Poll::Ready(Ok(()))
            } else {
                self.ctx.waiters.register(self.ctx.idx, cx);
                Poll::Pending
            }
        } else if self.ctx.waiters.can_check(self.ctx.idx, cx) {
            // SAFETY: `fut` never moves
            if let Some(ref mut fut) = self.as_mut().fut {
                let result = unsafe { Pin::new_unchecked(fut).poll(cx) };
                match result {
                    Poll::Pending => {
                        self.ctx.waiters.register(self.ctx.idx, cx);
                        return Poll::Pending;
                    }
                    Poll::Ready(res) => {
                        self.fut1 = res;
                        let _ = self.fut.take();
                    }
                }
            }

            if let Some(ref mut fut) = self.as_mut().fut1 {
                match unsafe { Pin::new_unchecked(fut).poll(cx) } {
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
                self.completed = true;
                self.ctx.waiters.notify();
                Poll::Ready(Ok(()))
            }
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, task::Poll};

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
            ctx.unready();
            #[allow(clippy::clone_on_copy)]
            let _ = ctx.clone();
            Ok(req)
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait()));
        let srv2 = srv1.clone();
        let mut rd = srv1.bind();

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        // set ready
        con.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 1);

        // set unready
        con.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 2);

        // no call in-between
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_ready_on_drop() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait()));

        let srv1 = srv.clone();
        let srv2 = srv1.clone();
        let mut rd = srv1.bind();

        let (tx, rx) = oneshot::channel();
        spawn(async move {
            select(rx, srv2.ready()).await;
            time::sleep(time::Millis(25000)).await;
            drop(srv2);
        });
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        let _ = tx.send(());
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        con.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_ready_after_shutdown() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait()));

        let srv1 = srv.clone();
        let srv2 = srv1.clone();
        let mut rd = srv1.bind();

        let (tx, rx) = oneshot::channel();
        spawn(async move {
            select(rx, srv2.ready()).await;
            srv2.shutdown().await;
            time::sleep(time::Millis(25000)).await;
            drop(srv2);
        });
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        let _ = tx.send(());
        time::sleep(time::Millis(250)).await;

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        con.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_shared_call() {
        let data = Rc::new(RefCell::new(Vec::new()));

        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait()));
        let srv2 = srv1.clone();
        let mut rd = srv1.bind();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);

        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let _ = srv1.ready().await;
            let i = srv1.call_nowait("srv1").await.unwrap();
            data1.borrow_mut().push(i);
        });

        let data2 = data.clone();
        ntex::rt::spawn(async move {
            let i = srv2.call("srv2").await.unwrap();
            data2.borrow_mut().push(i);
        });

        // set ready
        con.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        time::sleep(time::Millis(50)).await;
        assert_eq!(cnt.get(), 1);
        assert_eq!(&*data.borrow(), &["srv1"]);

        // set un-ready
        con.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 2);

        // set ready
        con.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        time::sleep(time::Millis(50)).await;
        assert_eq!(&*data.borrow(), &["srv1", "srv2"]);
    }
}
