use std::{cell::UnsafeCell, future::Future, marker, pin::Pin, rc::Rc, task};

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

impl Drop for Waiters {
    #[inline]
    fn drop(&mut self) {
        self.waiters.remove(self.index);
    }
}

impl<'a, S: ?Sized> ServiceCtx<'a, S> {
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

    #[inline]
    /// Wait for service readiness and then call service
    pub fn call<T, R>(&self, svc: &'a T, req: R) -> ServiceCall<'a, T, R>
    where
        T: Service<R> + ?Sized,
        R: 'a,
    {
        ServiceCall {
            state: ServiceCallState::Ready {
                svc,
                req: Some(req),
                idx: self.idx,
                waiters: self.waiters,
            },
        }
    }

    #[doc(hidden)]
    #[inline]
    /// Call service, do not check service readiness
    pub fn call_nowait<T, R>(&self, svc: &'a T, req: R) -> T::Future<'a>
    where
        T: Service<R> + ?Sized,
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
    }
}

impl<'a, S: ?Sized> Copy for ServiceCtx<'a, S> {}

impl<'a, S: ?Sized> Clone for ServiceCtx<'a, S> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            idx: self.idx,
            waiters: self.waiters,
            _t: marker::PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ServiceCall<'a, S, Req>
    where
        S: Service<Req>,
        S: 'a,
        S: ?Sized,
        Req: 'a,
    {
        #[pin]
        state: ServiceCallState<'a, S, Req>,
    }
}

pin_project_lite::pin_project! {
    #[project = ServiceCallStateProject]
    enum ServiceCallState<'a, S, Req>
    where
        S: Service<Req>,
        S: 'a,
        S: ?Sized,
        Req: 'a,
    {
        Ready { req: Option<Req>,
                svc: &'a S,
                idx: usize,
                waiters: &'a WaitersRef,
        },
        Call { #[pin] fut: S::Future<'a> },
        Empty,
    }
}

impl<'a, S, Req> Future for ServiceCall<'a, S, Req>
where
    S: Service<Req> + ?Sized,
{
    type Output = Result<S::Response, S::Error>;

    fn poll(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            ServiceCallStateProject::Ready {
                req,
                svc,
                idx,
                waiters,
            } => match svc.poll_ready(cx)? {
                task::Poll::Ready(()) => {
                    waiters.notify();

                    let fut = svc.call(
                        req.take().unwrap(),
                        ServiceCtx {
                            idx: *idx,
                            waiters,
                            _t: marker::PhantomData,
                        },
                    );
                    this.state.set(ServiceCallState::Call { fut });
                    self.poll(cx)
                }
                task::Poll::Pending => {
                    waiters.register(*idx, cx);
                    task::Poll::Pending
                }
            },
            ServiceCallStateProject::Call { fut } => fut.poll(cx).map(|r| {
                this.state.set(ServiceCallState::Empty);
                r
            }),
            ServiceCallStateProject::Empty => {
                panic!("future must not be polled after it returned `Poll::Ready`")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::{lazy, poll_fn, Ready};
    use ntex_util::{channel::condition, time};
    use std::{cell::Cell, cell::RefCell, rc::Rc, task::Context, task::Poll};

    use super::*;

    struct Srv(Rc<Cell<usize>>, condition::Waiter);

    impl Service<&'static str> for Srv {
        type Response = &'static str;
        type Error = ();
        type Future<'f> = Ready<Self::Response, ()>;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            self.1.poll_ready(cx).map(|_| Ok(()))
        }

        fn call<'a>(
            &'a self,
            req: &'static str,
            _: ServiceCtx<'a, Self>,
        ) -> Self::Future<'a> {
            Ready::Ok(req)
        }
    }

    #[ntex::test]
    async fn test_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();

        let srv1 = Container::from(Srv(cnt.clone(), con.wait()));
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

        let srv1 = Container::from(Srv(cnt.clone(), con.wait()));
        let srv2 = srv1.clone();

        let data1 = data.clone();
        ntex::rt::spawn(async move {
            let _ = poll_fn(|cx| srv1.poll_ready(cx)).await;
            let i = srv1.container_call("srv1").await.unwrap();
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

        assert_eq!(cnt.get(), 4);
        assert_eq!(&*data.borrow(), &["srv2"]);

        con.notify();
        time::sleep(time::Millis(150)).await;

        assert_eq!(cnt.get(), 5);
        assert_eq!(&*data.borrow(), &["srv2", "srv1"]);
    }
}
