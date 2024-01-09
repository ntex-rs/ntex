use std::{cell::UnsafeCell, fmt, future::poll_fn, marker, rc::Rc, task, task::Poll};

use crate::Service;

pub struct ServiceCtx<'a, S: ?Sized> {
    idx: usize,
    waiters: &'a WaitersRef,
    _t: marker::PhantomData<Rc<S>>,
}

pub(crate) struct WaitersRef(UnsafeCell<slab::Slab<Option<task::Waker>>>);

pub(crate) struct Waiters {
    index: usize,
    waiters: Rc<WaitersRef>,
}

impl WaitersRef {
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut slab::Slab<Option<task::Waker>> {
        unsafe { &mut *self.0.get() }
    }

    fn insert(&self) -> usize {
        self.get().insert(None)
    }

    fn remove(&self, idx: usize) {
        self.notify();
        self.get().remove(idx);
    }

    fn register(&self, idx: usize, cx: &mut task::Context<'_>) {
        self.get()[idx] = Some(cx.waker().clone());
    }

    fn notify(&self) {
        for (_, waker) in self.get().iter_mut() {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    }
}

impl Waiters {
    pub(crate) fn new() -> Self {
        let mut waiters = slab::Slab::new();
        let index = waiters.insert(None);
        Waiters {
            index,
            waiters: Rc::new(WaitersRef(UnsafeCell::new(waiters))),
        }
    }

    pub(crate) fn get_ref(&self) -> &WaitersRef {
        self.waiters.as_ref()
    }

    pub(crate) fn register(&self, cx: &mut task::Context<'_>) {
        self.waiters.register(self.index, cx)
    }

    pub(crate) fn notify(&self) {
        self.waiters.notify()
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

impl Drop for Waiters {
    #[inline]
    fn drop(&mut self) {
        self.waiters.remove(self.index);
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

    pub(crate) fn from_ref(idx: usize, waiters: &'a WaitersRef) -> Self {
        Self {
            idx,
            waiters,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn inner(self) -> (usize, &'a WaitersRef) {
        (self.idx, self.waiters)
    }

    /// Returns when the service is able to process requests.
    pub async fn ready<T, R>(&self, svc: &'a T) -> Result<(), T::Error>
    where
        T: Service<R>,
    {
        // check readiness and notify waiters
        poll_fn(move |cx| match svc.poll_ready(cx)? {
            Poll::Ready(()) => {
                self.waiters.notify();
                Poll::Ready(Ok(()))
            }
            Poll::Pending => {
                self.waiters.register(self.idx, cx);
                Poll::Pending
            }
        })
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

#[cfg(test)]
mod tests {
    use ntex_util::future::lazy;
    use ntex_util::{channel::condition, time};
    use std::task::{Context, Poll};
    use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc};

    use super::*;
    use crate::Pipeline;

    struct Srv(Rc<Cell<usize>>, condition::Waiter);

    impl Service<&'static str> for Srv {
        type Response = &'static str;
        type Error = ();

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            self.1.poll_ready(cx).map(|_| Ok(()))
        }

        async fn call(
            &self,
            req: &'static str,
            ctx: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, Self::Error> {
            format!("{:?}", ctx);
            let _ = ctx.clone();
            Ok(req)
        }
    }

    #[ntex::test]
    async fn test_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait()));
        let srv2 = srv1.clone();

        let res = lazy(|cx| srv1.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 1);

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 2);

        con.notify();

        let res = lazy(|cx| srv1.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 3);

        let res = lazy(|cx| srv2.poll_ready(cx)).await;
        assert_eq!(res, Poll::Pending);
        assert_eq!(cnt.get(), 4);
    }

    #[ntex::test]
    async fn test_shared_call() {
        let data = Rc::new(RefCell::new(Vec::new()));

        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Pipeline::from(Srv(cnt.clone(), con.wait()));
        let srv2 = srv1.clone();

        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let _ = poll_fn(|cx| srv1.poll_ready(cx)).await;
            let i = srv1.call_nowait("srv1").await.unwrap();
            data1.borrow_mut().push(i);
        });

        let data2 = data.clone();
        ntex::rt::spawn(async move {
            let i = srv2.call_static("srv2").await.unwrap();
            data2.borrow_mut().push(i);
        });
        time::sleep(time::Millis(50)).await;

        con.notify();
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 4);
        assert_eq!(&*data.borrow(), &["srv2"]);

        con.notify();
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 5);
        assert_eq!(&*data.borrow(), &["srv2", "srv1"]);
    }
}
