use std::{
    future::Future, marker::PhantomData, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use super::{Service, ServiceFactory};

/// Service for the `and_then` combinator, chaining a computation onto the end
/// of another service which completes successfully.
///
/// This is created by the `ServiceExt::and_then` method.
pub struct AndThen<A, B, Req>(Rc<(A, B)>, PhantomData<fn(Req)>);

impl<A, B, Req> AndThen<A, B, Req> {
    /// Create new `AndThen` combinator
    pub(crate) fn new(a: A, b: B) -> Self
    where
        A: Service<Req>,
        B: Service<A::Response, Error = A::Error>,
    {
        Self(Rc::new((a, b)), PhantomData)
    }
}

impl<A, B, Req> Clone for AndThen<A, B, Req> {
    fn clone(&self) -> Self {
        AndThen(self.0.clone(), PhantomData)
    }
}

impl<A, B, Req> Service<Req> for AndThen<A, B, Req>
where
    A: Service<Req>,
    B: Service<A::Response, Error = A::Error>,
{
    type Response = B::Response;
    type Error = A::Error;
    type Future = AndThenServiceResponse<A, B, Req>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let srv = self.0.as_ref();
        let not_ready = !srv.0.poll_ready(cx)?.is_ready();
        if !srv.1.poll_ready(cx)?.is_ready() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn shutdown(&self) {
        let srv = self.0.as_ref();
        srv.0.shutdown();
        srv.1.shutdown();
    }

    #[inline]
    fn call(&self, req: Req) -> Self::Future {
        AndThenServiceResponse {
            state: State::A {
                fut: self.0.as_ref().0.call(req),
                b: Some(self.0.clone()),
            },
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AndThenServiceResponse<A, B, Req>
    where
        A: Service<Req>,
        B: Service<A::Response, Error = A::Error>,
    {
        #[pin]
        state: State<A, B, Req>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<A, B, Req>
    where
        A: Service<Req>,
        B: Service<A::Response, Error = A::Error>,
    {
        A { #[pin] fut: A::Future, b: Option<Rc<(A, B)>> },
        B { #[pin] fut: B::Future },
        Empty,
    }
}

impl<A, B, Req> Future for AndThenServiceResponse<A, B, Req>
where
    A: Service<Req>,
    B: Service<A::Response, Error = A::Error>,
{
    type Output = Result<B::Response, A::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            StateProject::A { fut, b } => match fut.poll(cx)? {
                Poll::Ready(res) => {
                    let b = b.take().unwrap();
                    this.state.set(State::Empty); // drop fut A
                    let fut = b.as_ref().1.call(res);
                    this.state.set(State::B { fut });
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            StateProject::B { fut } => fut.poll(cx).map(|r| {
                this.state.set(State::Empty);
                r
            }),
            StateProject::Empty => {
                panic!("future must not be polled after it returned `Poll::Ready`")
            }
        }
    }
}

/// `.and_then()` service factory combinator
pub struct AndThenFactory<A, B, Req> {
    inner: Rc<(A, B)>,
    _t: PhantomData<fn(Req)>,
}

impl<A, B, Req> AndThenFactory<A, B, Req>
where
    A: ServiceFactory<Req>,
    A::Config: Clone,
    B: ServiceFactory<
        A::Response,
        Config = A::Config,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    /// Create new `AndThenFactory` combinator
    pub(crate) fn new(a: A, b: B) -> Self {
        Self {
            inner: Rc::new((a, b)),
            _t: PhantomData,
        }
    }
}

impl<A, B, Req> ServiceFactory<Req> for AndThenFactory<A, B, Req>
where
    A: ServiceFactory<Req>,
    A::Config: Clone,
    B: ServiceFactory<
        A::Response,
        Config = A::Config,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    type Response = B::Response;
    type Error = A::Error;

    type Config = A::Config;
    type Service = AndThen<A::Service, B::Service, Req>;
    type InitError = A::InitError;
    type Future = AndThenServiceFactoryResponse<A, B, Req>;

    fn new_service(&self, cfg: A::Config) -> Self::Future {
        let inner = &*self.inner;
        AndThenServiceFactoryResponse::new(
            inner.0.new_service(cfg.clone()),
            inner.1.new_service(cfg),
        )
    }
}

impl<A, B, Req> Clone for AndThenFactory<A, B, Req> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _t: PhantomData,
        }
    }
}

pin_project_lite::pin_project! {
    pub struct AndThenServiceFactoryResponse<A, B, Req>
    where
        A: ServiceFactory<Req>,
        B: ServiceFactory<A::Response>,
    {
        #[pin]
        fut_a: A::Future,
        #[pin]
        fut_b: B::Future,

        a: Option<A::Service>,
        b: Option<B::Service>,
    }
}

impl<A, B, Req> AndThenServiceFactoryResponse<A, B, Req>
where
    A: ServiceFactory<Req>,
    B: ServiceFactory<A::Response>,
{
    fn new(fut_a: A::Future, fut_b: B::Future) -> Self {
        AndThenServiceFactoryResponse {
            fut_a,
            fut_b,
            a: None,
            b: None,
        }
    }
}

impl<A, B, Req> Future for AndThenServiceFactoryResponse<A, B, Req>
where
    A: ServiceFactory<Req>,
    B: ServiceFactory<A::Response, Error = A::Error, InitError = A::InitError>,
{
    type Output = Result<AndThen<A::Service, B::Service, Req>, A::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if this.a.is_none() {
            if let Poll::Ready(service) = this.fut_a.poll(cx)? {
                *this.a = Some(service);
            }
        }
        if this.b.is_none() {
            if let Poll::Ready(service) = this.fut_b.poll(cx)? {
                *this.b = Some(service);
            }
        }
        if this.a.is_some() && this.b.is_some() {
            Poll::Ready(Ok(AndThen::new(
                this.a.take().unwrap(),
                this.b.take().unwrap(),
            )))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, task::Context, task::Poll};

    use crate::{fn_factory, pipeline, pipeline_factory, Service, ServiceFactory};
    use ntex_util::future::{lazy, Ready};

    struct Srv1(Rc<Cell<usize>>);

    impl Service<&'static str> for Srv1 {
        type Response = &'static str;
        type Error = ();
        type Future = Ready<Self::Response, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call(&self, req: &'static str) -> Self::Future {
            Ready::Ok(req)
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>);

    impl Service<&'static str> for Srv2 {
        type Response = (&'static str, &'static str);
        type Error = ();
        type Future = Ready<Self::Response, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call(&self, req: &'static str) -> Self::Future {
            Ready::Ok((req, "srv2"))
        }
    }

    #[ntex::test]
    async fn test_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let srv = pipeline(Srv1(cnt.clone()))
            .and_then(Srv2(cnt.clone()))
            .clone();
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 2);

        srv.shutdown();
    }

    #[ntex::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let srv = pipeline(Srv1(cnt.clone())).and_then(Srv2(cnt));
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }

    #[ntex::test]
    async fn test_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let new_srv = pipeline_factory(fn_factory(move || {
            Ready::from(Ok::<_, ()>(Srv1(cnt2.clone())))
        }))
        .and_then(move || Ready::from(Ok(Srv2(cnt.clone()))))
        .clone();

        let srv = new_srv.new_service(()).await.unwrap();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }
}
