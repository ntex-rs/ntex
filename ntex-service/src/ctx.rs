use std::task::{Context, Poll, Waker};
use std::{cell, fmt, future::Future, marker, pin::Pin, rc::Rc};

use crate::Service;

pub struct ServiceCtx<'a, S: ?Sized> {
    idx: u32,
    waiters: &'a WaitersRef,
    _t: marker::PhantomData<Rc<S>>,
}

pub(crate) struct WaitersRef {
    pub(crate) cur: cell::Cell<u32>,
    pub(crate) indexes: cell::UnsafeCell<slab::Slab<Option<Waker>>>,
    pub(crate) readiness: cell::UnsafeCell<slab::Slab<Option<Waker>>>,
}

impl WaitersRef {
    pub(crate) fn new() -> (u32, Self) {
        let mut waiters = slab::Slab::new();
        let index = waiters.insert(None) as u32;

        (index, WaitersRef {
            cur: cell::Cell::new(u32::MAX),
            indexes: cell::UnsafeCell::new(waiters),
            readiness: cell::UnsafeCell::new(slab::Slab::new()),
        })
    }

    #[allow(clippy::mut_from_ref)]
    pub(crate) fn get(&self) -> &mut slab::Slab<Option<Waker>> {
        unsafe { &mut *self.indexes.get() }
    }

    pub(crate) fn insert(&self) -> u32 {
        self.get().insert(None) as u32
    }

    pub(crate) fn remove(&self, idx: u32) {
        self.notify();
        self.get().remove(idx as usize);
    }

    pub(crate) fn remove_index(&self, idx: u32) {
        self.get().remove(idx as usize);

        if self.cur.get() == idx {
            self.notify();
        }
    }

    pub(crate) fn register(&self, idx: u32, cx: &mut Context<'_>) {
        self.get()[idx as usize] = Some(cx.waker().clone());
    }

    pub(crate) fn notify(&self) {
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

    // /// Returns when the service is able to process requests.
    // pub async fn check_readiness<T, R>(&self, svc: &'a T) -> Result<(), T::Error>
    // where
    //     T: Service<R>,
    // {
    //     // active readiness
    //     loop {
    //         // check readiness and notify waiters
    //         let completed = ReadyCall {
    //             fut: Some(svc.ready()),
    //             fut1: None,
    //             ctx: *self,
    //             completed: false,
    //             _t: marker::PhantomData,
    //         }
    //         .await?;

    //         if completed {
    //             return Ok(())
    //         }
    //     }
    // }

    #[inline]
    /// Wait for service readiness and then call service
    pub async fn call<T, R>(&self, svc: &'a T, req: R) -> Result<T::Response, T::Error>
    where
        T: Service<R>,
        R: 'a,
    {
        // self.check_readiness(svc).await?;

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
        if !self.completed {
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
    type Output = Result<bool, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        println!("-- POLL -- {:?}", self.ctx.waiters.can_check(self.ctx.idx, cx));

        if self.ctx.waiters.can_check(self.ctx.idx, cx) {
            if let Some(ref mut fut) = self.as_mut().fut {
                // SAFETY: `fut` never moves
                match unsafe { Pin::new_unchecked(fut).poll(cx) } {
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
                println!("poll -------- 1");
                match unsafe { Pin::new_unchecked(fut).poll(cx) } {
                    Poll::Pending => {
                        // ready future is pending
                        self.completed = true;
                        self.ctx.waiters.notify();
                        Poll::Ready(Ok(true))
                    }
                    Poll::Ready(Err(e)) => {
                        self.completed = true;
                        self.ctx.waiters.notify();
                        Poll::Ready(Err(e))
                    }
                    Poll::Ready(Ok(())) => {
                        self.completed = true;
                        Poll::Ready(Ok(false))
                    }
                }
            } else {
                self.completed = true;
                self.ctx.waiters.notify();
                Poll::Ready(Ok(true))
            }
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, task::Poll, future::poll_fn};

    use ntex_util::channel::{condition, oneshot};
    use ntex_util::{future::lazy, future::select, spawn, time};

    use super::*;
    use crate::Pipeline;

    struct Srv(Rc<Cell<usize>>, condition::Waiter, condition::Condition);

    impl Service<&'static str> for Srv {
        type Response = &'static str;
        type Error = ();

        async fn ready(&self) -> Option<impl Future<Output = Result<(), Self::Error>>> {
            self.0.set(self.0.get() + 1);
            self.1.ready().await;
            Some(async move {
                self.2.wait().await;
                Ok(())
            })
        }

        async fn call(
            &self,
            req: &'static str,
            ctx: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, Self::Error> {
            let _ = format!("{:?}", ctx);
            self.2.notify();
            #[allow(clippy::clone_on_copy)]
            let _ = ctx.clone();
            Ok(req)
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let ready = condition::Condition::new();
        let not_ready = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), ready.wait(), not_ready.clone()));
        let _srv2 = srv1.clone();
        let mut rd = srv1.bind();

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        // set ready
        ready.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 1);

        // set unready
        not_ready.notify();
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
        let ready = condition::Condition::new();
        let not_ready = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), ready.wait(), not_ready.clone()));

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

        ready.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_ready_after_shutdown() {
        let cnt = Rc::new(Cell::new(0));
        let ready = condition::Condition::new();
        let not_ready = condition::Condition::new();

        let srv = Pipeline::from(Srv(cnt.clone(), ready.wait(), not_ready.clone()));

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

        ready.notify();
        let res = lazy(|cx| rd.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn test_bound_shared_call() {
        let data = Rc::new(RefCell::new(Vec::new()));

        let cnt = Rc::new(Cell::new(0));
        let ready = condition::Condition::new();
        let not_ready = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), ready.wait(), not_ready.clone()));
        let srv2 = srv1.clone();
        let mut rd = srv1.bind();

        ntex::rt::spawn(async move {
            loop {
                let _ = poll_fn(|cx| rd.poll_ready(cx)).await;
                time::sleep(time::Millis(25)).await;
            }
        });

        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let i = srv2.call_nowait("srv1").await.unwrap();
            data1.borrow_mut().push(i);
        });

        let srv3 = srv1.clone();
        let data2 = data.clone();
        ntex::rt::spawn(async move {
            let i = srv3.call("srv2").await.unwrap();
            data2.borrow_mut().push(i);
        });
        time::sleep(time::Millis(50)).await;

        ready.notify();
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 2);
        assert_eq!(&*data.borrow(), &["srv1"]);

        not_ready.notify();
        time::sleep(time::Millis(150)).await;
        assert_eq!(cnt.get(), 2);
        assert_eq!(&*data.borrow(), &["srv1"]);

        println!("1 =======================================");
        ready.notify();
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 3);
        println!("2 ========  {:?}", &*data.borrow());

        assert_eq!(&*data.borrow(), &["srv1", "srv2"]);
    }
}
