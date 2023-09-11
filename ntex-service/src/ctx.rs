use std::{cell::UnsafeCell, fmt, future::Future, marker, pin::Pin, rc::Rc, task};

use crate::{Pipeline, Service};

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

    #[inline]
    /// Wait for service readiness and then call service
    pub fn call<T, R>(&self, svc: &'a T, req: R) -> ServiceCall<'a, T, R>
    where
        T: Service<R>,
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

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ServiceCall<'a, S, Req>
    where
        S: Service<Req>,
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
        Req: 'a,
    {
        Ready { req: Option<Req>,
                svc: &'a S,
                idx: usize,
                waiters: &'a WaitersRef,
        },
        ReadyPl { req: Option<Req>,
                  svc: &'a Pipeline<S>,
                  pl: Pipeline<S>,
        },
        Call { #[pin] fut: S::Future<'a> },
        Empty,
    }
}

impl<'a, S, Req> ServiceCall<'a, S, Req>
where
    S: Service<Req>,
    Req: 'a,
{
    pub(crate) fn call_pipeline(req: Req, svc: &'a Pipeline<S>) -> Self {
        ServiceCall {
            state: ServiceCallState::ReadyPl {
                req: Some(req),
                pl: svc.clone(),
                svc,
            },
        }
    }

    pub fn advance_to_call(self) -> ServiceCallToCall<'a, S, Req> {
        match self.state {
            ServiceCallState::Ready { .. } | ServiceCallState::ReadyPl { .. } => {}
            ServiceCallState::Call { .. } | ServiceCallState::Empty => {
                panic!(
                    "`ServiceCall::advance_to_call` must be called before `ServiceCall::poll`"
                )
            }
        }
        ServiceCallToCall { state: self.state }
    }
}

impl<'a, S, Req> Future for ServiceCall<'a, S, Req>
where
    S: Service<Req>,
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
                            waiters,
                            idx: *idx,
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
            ServiceCallStateProject::ReadyPl { req, svc, pl } => {
                task::ready!(pl.poll_ready(cx))?;

                let ctx = ServiceCtx::new(&svc.waiters);
                let svc_call = svc.get_ref().call(req.take().unwrap(), ctx);

                // SAFETY: `svc_call` has same lifetime same as lifetime of `pl.svc`
                // Pipeline::svc is heap allocated(Rc<S>), we keep it alive until
                // `svc_call` get resolved to result
                let fut = unsafe { std::mem::transmute(svc_call) };

                this.state.set(ServiceCallState::Call { fut });
                self.poll(cx)
            }
            ServiceCallStateProject::Call { fut, .. } => fut.poll(cx).map(|r| {
                this.state.set(ServiceCallState::Empty);
                r
            }),
            ServiceCallStateProject::Empty => {
                panic!("future must not be polled after it returned `Poll::Ready`")
            }
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ServiceCallToCall<'a, S, Req>
    where
        S: Service<Req>,
        Req: 'a,
    {
        #[pin]
        state: ServiceCallState<'a, S, Req>,
    }
}

impl<'a, S, Req> Future for ServiceCallToCall<'a, S, Req>
where
    S: Service<Req>,
{
    type Output = Result<S::Future<'a>, S::Error>;

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
                            waiters,
                            idx: *idx,
                            _t: marker::PhantomData,
                        },
                    );
                    this.state.set(ServiceCallState::Empty);
                    task::Poll::Ready(Ok(fut))
                }
                task::Poll::Pending => {
                    waiters.register(*idx, cx);
                    task::Poll::Pending
                }
            },
            ServiceCallStateProject::ReadyPl { req, svc, pl } => {
                task::ready!(pl.poll_ready(cx))?;

                let ctx = ServiceCtx::new(&svc.waiters);
                task::Poll::Ready(Ok(svc.get_ref().call(req.take().unwrap(), ctx)))
            }
            ServiceCallStateProject::Call { .. } => {
                unreachable!("`ServiceCallToCall` can only be constructed in `Ready` state")
            }
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
    use crate::Pipeline;

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
            ctx: ServiceCtx<'a, Self>,
        ) -> Self::Future<'a> {
            let _ = ctx.clone();
            Ready::Ok(req)
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

    #[ntex::test]
    async fn test_advance_to_call() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait()));

        let mut fut = srv.call("test").advance_to_call();
        let _ = lazy(|cx| Pin::new(&mut fut).poll(cx)).await;
        con.notify();

        let res = lazy(|cx| Pin::new(&mut fut).poll(cx)).await;
        assert!(res.is_ready());
    }

    #[ntex::test]
    #[should_panic]
    async fn test_advance_to_call_panic() {
        let cnt = Rc::new(Cell::new(0));
        let con = condition::Condition::new();
        let srv = Pipeline::from(Srv(cnt.clone(), con.wait()));

        let mut fut = srv.call("test");
        let _ = lazy(|cx| Pin::new(&mut fut).poll(cx)).await;
        con.notify();

        let _ = lazy(|cx| Pin::new(&mut fut).poll(cx)).await;
        let _f = fut.advance_to_call();
    }
}
