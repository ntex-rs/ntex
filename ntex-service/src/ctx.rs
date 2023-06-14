use std::{cell::RefCell, future::Future, marker, pin::Pin, rc::Rc, task, task::Poll};

use crate::{Service, ServiceFactory};

pub struct Container<S, R> {
    svc: Rc<S>,
    index: usize,
    waiters: Rc<RefCell<slab::Slab<Option<task::Waker>>>>,
    _t: marker::PhantomData<R>,
}

impl<S, R> Container<S, R>
where
    S: Service<R>,
{
    #[inline]
    pub fn new(svc: S) -> Self {
        let mut waiters = slab::Slab::new();
        let index = waiters.insert(None);
        Container {
            index,
            svc: Rc::new(svc),
            waiters: Rc::new(RefCell::new(waiters)),
            _t: marker::PhantomData,
        }
    }

    #[inline]
    /// Returns `Ready` when the service is able to process requests.
    pub fn poll_ready(&self, cx: &mut task::Context<'_>) -> Poll<Result<(), S::Error>> {
        let res = self.svc.poll_ready(cx);

        if res.is_pending() {
            self.waiters.borrow_mut()[self.index] = Some(cx.waker().clone());
        }
        res
    }

    #[inline]
    /// Shutdown enclosed service.
    pub fn poll_shutdown(&self, cx: &mut task::Context<'_>) -> Poll<()> {
        self.svc.poll_shutdown(cx)
    }

    #[inline]
    /// Process the request and return the response asynchronously.
    pub fn call<'a>(&'a self, req: R) -> ServiceCall<'a, S, R> {
        let ctx = Ctx::<'a, S> {
            index: self.index,
            waiters: &self.waiters,
            _t: marker::PhantomData,
        };
        ctx.call(self.svc.as_ref(), req)
    }

    pub(crate) fn create<F: ServiceFactory<R, C>, C>(
        f: &F,
        cfg: C,
    ) -> ContainerFactory<'_, F, R, C> {
        ContainerFactory {
            fut: f.create(cfg),
            _t: marker::PhantomData,
        }
    }
}

impl<S, R> Clone for Container<S, R> {
    fn clone(&self) -> Self {
        let index = self.waiters.borrow_mut().insert(None);

        Self {
            index,
            svc: self.svc.clone(),
            waiters: self.waiters.clone(),
            _t: marker::PhantomData,
        }
    }
}

impl<S, R> From<S> for Container<S, R>
where
    S: Service<R>,
{
    fn from(svc: S) -> Self {
        Container::new(svc)
    }
}

impl<S, R> Drop for Container<S, R> {
    fn drop(&mut self) {
        self.waiters.borrow_mut().remove(self.index);
    }
}

pub struct Ctx<'b, S: ?Sized> {
    index: usize,
    waiters: &'b Rc<RefCell<slab::Slab<Option<task::Waker>>>>,
    _t: marker::PhantomData<Rc<S>>,
}

impl<'b, S: ?Sized> Ctx<'b, S> {
    pub(crate) fn new(
        index: usize,
        waiters: &'b Rc<RefCell<slab::Slab<Option<task::Waker>>>>,
    ) -> Self {
        Self {
            index,
            waiters,
            _t: marker::PhantomData,
        }
    }

    pub(crate) fn into_inner(
        self,
    ) -> (usize, &'b Rc<RefCell<slab::Slab<Option<task::Waker>>>>) {
        (self.index, self.waiters)
    }

    /// Call service, do not check service readiness
    pub(crate) fn call_nowait<T, R>(&self, svc: &'b T, req: R) -> T::Future<'b>
    where
        T: Service<R> + ?Sized,
        R: 'b,
    {
        svc.call(
            req,
            Ctx {
                index: self.index,
                waiters: self.waiters,
                _t: marker::PhantomData,
            },
        )
    }

    /// Wait for service readiness and then call service
    pub fn call<T, R>(&self, svc: &'b T, req: R) -> ServiceCall<'b, T, R>
    where
        T: Service<R> + ?Sized,
        R: 'b,
    {
        ServiceCall {
            svc,
            index: self.index,
            waiters: self.waiters,
            state: ServiceCallState::Ready { req: Some(req) },
        }
    }
}

impl<'b, S: ?Sized> Clone for Ctx<'b, S> {
    fn clone(&self) -> Self {
        Self {
            index: self.index,
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
        svc: &'a T,
        index: usize,
        waiters: &'a Rc<RefCell<slab::Slab<Option<task::Waker>>>>,
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
        Ready { req: Option<Req> },
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
            ServiceCallStateProject::Ready { req } => match this.svc.poll_ready(cx)? {
                Poll::Ready(()) => {
                    for (_, waker) in &mut *this.waiters.borrow_mut() {
                        if let Some(waker) = waker.take() {
                            waker.wake();
                        }
                    }

                    let fut = this.svc.call(
                        req.take().unwrap(),
                        Ctx {
                            index: *this.index,
                            waiters: this.waiters,
                            _t: marker::PhantomData,
                        },
                    );
                    this.state.set(ServiceCallState::Call { fut });
                    self.poll(cx)
                }
                Poll::Pending => {
                    this.waiters.borrow_mut()[*this.index] = Some(cx.waker().clone());
                    Poll::Pending
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
        _t: marker::PhantomData<(R, C)>,
    }
}

impl<'f, F, R, C> Future for ContainerFactory<'f, F, R, C>
where
    F: ServiceFactory<R, C> + 'f,
{
    type Output = Result<Container<F::Service, R>, F::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(Ok(Container::new(task::ready!(self
            .project()
            .fut
            .poll(cx))?)))
    }
}
