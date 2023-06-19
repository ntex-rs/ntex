use std::{
    cell::UnsafeCell, future::Future, marker, ops, pin::Pin, rc::Rc, task, task::Poll,
};

use crate::{Service, ServiceFactory};

/// Container for a service.
///
/// Container allows to call enclosed service and adds support of shared readiness.
pub struct Container<S> {
    svc: Rc<S>,
    waiters: Waiters,
}

pub struct ServiceCtx<'a, S: ?Sized> {
    waiters: &'a Waiters,
    _t: marker::PhantomData<Rc<S>>,
}

pub(crate) struct Waiters {
    index: usize,
    waiters: Rc<UnsafeCell<slab::Slab<Option<task::Waker>>>>,
}

impl Waiters {
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut slab::Slab<Option<task::Waker>> {
        unsafe { &mut *self.waiters.as_ref().get() }
    }

    fn notify(&self) {
        for (_, waker) in self.get().iter_mut() {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    }

    fn register(&self, cx: &mut task::Context<'_>) {
        self.get()[self.index] = Some(cx.waker().clone());
    }
}

impl Clone for Waiters {
    fn clone(&self) -> Self {
        Waiters {
            index: self.get().insert(None),
            waiters: self.waiters.clone(),
        }
    }
}

impl Drop for Waiters {
    #[inline]
    fn drop(&mut self) {
        self.get().remove(self.index);
        self.notify();
    }
}

impl<S> Container<S> {
    #[inline]
    /// Construct new container instance.
    pub fn new(svc: S) -> Self {
        let mut waiters = slab::Slab::new();
        let index = waiters.insert(None);
        Container {
            svc: Rc::new(svc),
            waiters: Waiters {
                index,
                waiters: Rc::new(UnsafeCell::new(waiters)),
            },
        }
    }

    #[inline]
    /// Returns `Ready` when the service is able to process requests.
    pub fn poll_ready<R>(&self, cx: &mut task::Context<'_>) -> Poll<Result<(), S::Error>>
    where
        S: Service<R>,
    {
        let res = self.svc.poll_ready(cx);
        if res.is_pending() {
            self.waiters.register(cx)
        } else {
            self.waiters.notify()
        }
        res
    }

    #[inline]
    /// Shutdown enclosed service.
    pub fn poll_shutdown<R>(&self, cx: &mut task::Context<'_>) -> Poll<()>
    where
        S: Service<R>,
    {
        self.svc.poll_shutdown(cx)
    }

    #[inline]
    /// Process the request and return the response asynchronously.
    pub fn call<'a, R>(&'a self, req: R) -> ServiceCall<'a, S, R>
    where
        S: Service<R>,
    {
        let ctx = ServiceCtx::<'a, S> {
            waiters: &self.waiters,
            _t: marker::PhantomData,
        };
        ctx.call(self.svc.as_ref(), req)
    }

    pub(crate) fn create<F: ServiceFactory<R, C>, R, C>(
        f: &F,
        cfg: C,
    ) -> ContainerFactory<'_, F, R, C> {
        ContainerFactory { fut: f.create(cfg) }
    }

    /// Extract service if container hadnt been cloned before.
    pub fn into_service(self) -> Option<S> {
        let svc = self.svc.clone();
        drop(self);
        Rc::try_unwrap(svc).ok()
    }
}

impl<S> Clone for Container<S> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            svc: self.svc.clone(),
            waiters: self.waiters.clone(),
        }
    }
}

impl<S> From<S> for Container<S> {
    #[inline]
    fn from(svc: S) -> Self {
        Container::new(svc)
    }
}

impl<S> ops::Deref for Container<S> {
    type Target = S;

    #[inline]
    fn deref(&self) -> &S {
        self.svc.as_ref()
    }
}

impl<'a, S: ?Sized> ServiceCtx<'a, S> {
    pub(crate) fn new(waiters: &'a Waiters) -> Self {
        Self {
            waiters,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn waiters(self) -> &'a Waiters {
        self.waiters
    }

    /// Call service, do not check service readiness
    pub(crate) fn call_nowait<T, R>(&self, svc: &'a T, req: R) -> T::Future<'a>
    where
        T: Service<R> + ?Sized,
        R: 'a,
    {
        svc.call(
            req,
            ServiceCtx {
                waiters: self.waiters,
                _t: marker::PhantomData,
            },
        )
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
                waiters: self.waiters,
            },
        }
    }
}

impl<'a, S: ?Sized> Copy for ServiceCtx<'a, S> {}

impl<'a, S: ?Sized> Clone for ServiceCtx<'a, S> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            waiters: self.waiters,
            _t: marker::PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ServiceCall<'a, T, Req>
    where
        T: Service<Req>,
        T: 'a,
        T: ?Sized,
        Req: 'a,
    {
        #[pin]
        state: ServiceCallState<'a, T, Req>,
    }
}

pin_project_lite::pin_project! {
    #[project = ServiceCallStateProject]
    enum ServiceCallState<'a, T, Req>
    where
        T: Service<Req>,
        T: 'a,
        T: ?Sized,
        Req: 'a,
    {
        Ready { req: Option<Req>,
                svc: &'a T,
                waiters: &'a Waiters,
        },
        Call { #[pin] fut: T::Future<'a> },
        Empty,
    }
}

impl<'a, T, Req> Future for ServiceCall<'a, T, Req>
where
    T: Service<Req> + ?Sized,
{
    type Output = Result<T::Response, T::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            ServiceCallStateProject::Ready { req, svc, waiters } => {
                match svc.poll_ready(cx)? {
                    Poll::Ready(()) => {
                        waiters.notify();

                        let fut = svc.call(
                            req.take().unwrap(),
                            ServiceCtx {
                                waiters,
                                _t: marker::PhantomData,
                            },
                        );
                        this.state.set(ServiceCallState::Call { fut });
                        self.poll(cx)
                    }
                    Poll::Pending => {
                        waiters.register(cx);
                        Poll::Pending
                    }
                }
            }
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

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ContainerFactory<'f, F, R, C>
    where F: ServiceFactory<R, C>,
          F: ?Sized,
          F: 'f,
          C: 'f,
    {
        #[pin]
        fut: F::Future<'f>,
    }
}

impl<'f, F, R, C> Future for ContainerFactory<'f, F, R, C>
where
    F: ServiceFactory<R, C> + 'f,
{
    type Output = Result<Container<F::Service>, F::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(Container::new(task::ready!(self
            .project()
            .fut
            .poll(cx))?)))
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::{channel::condition, future::lazy, future::Ready, time};
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
            let i = srv1.call("srv1").await.unwrap();
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
