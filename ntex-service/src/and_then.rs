use std::{future::Future, pin::Pin, rc::Rc, task::Context, task::Poll};

use super::{Service, ServiceFactory};

/// Service for the `and_then` combinator, chaining a computation onto the end
/// of another service which completes successfully.
///
/// This is created by the `ServiceExt::and_then` method.
pub(crate) struct AndThenService<A, B>(Rc<(A, B)>);

impl<A, B> AndThenService<A, B> {
    /// Create new `AndThen` combinator
    pub(crate) fn new(a: A, b: B) -> Self
    where
        A: Service,
        B: Service<Request = A::Response, Error = A::Error>,
    {
        Self(Rc::new((a, b)))
    }
}

impl<A, B> Clone for AndThenService<A, B> {
    fn clone(&self) -> Self {
        AndThenService(self.0.clone())
    }
}

impl<A, B> Service for AndThenService<A, B>
where
    A: Service,
    B: Service<Request = A::Response, Error = A::Error>,
{
    type Request = A::Request;
    type Response = B::Response;
    type Error = A::Error;
    type Future = AndThenServiceResponse<A, B>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let srv = self.0.as_ref();
        let not_ready = !srv.0.poll_ready(cx)?.is_ready();
        if !srv.1.poll_ready(cx)?.is_ready() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let srv = self.0.as_ref();

        if srv.0.poll_shutdown(cx, is_error).is_ready()
            && srv.1.poll_shutdown(cx, is_error).is_ready()
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call(&self, req: A::Request) -> Self::Future {
        AndThenServiceResponse {
            state: State::A {
                fut: self.0.as_ref().0.call(req),
                b: Some(self.0.clone()),
            },
        }
    }
}

pin_project_lite::pin_project! {
    pub(crate) struct AndThenServiceResponse<A, B>
    where
        A: Service,
        B: Service<Request = A::Response, Error = A::Error>,
    {
        #[pin]
        state: State<A, B>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<A, B>
    where
        A: Service,
        B: Service<Request = A::Response, Error = A::Error>,
    {
        A { #[pin] fut: A::Future, b: Option<Rc<(A, B)>> },
        B { #[pin] fut: B::Future },
        Empty,
    }
}

impl<A, B> Future for AndThenServiceResponse<A, B>
where
    A: Service,
    B: Service<Request = A::Response, Error = A::Error>,
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
pub(crate) struct AndThenServiceFactory<A, B>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<
        Config = A::Config,
        Request = A::Response,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    inner: Rc<(A, B)>,
}

impl<A, B> AndThenServiceFactory<A, B>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<
        Config = A::Config,
        Request = A::Response,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    /// Create new `AndThenFactory` combinator
    pub(crate) fn new(a: A, b: B) -> Self {
        Self {
            inner: Rc::new((a, b)),
        }
    }
}

impl<A, B> ServiceFactory for AndThenServiceFactory<A, B>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<
        Config = A::Config,
        Request = A::Response,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    type Request = A::Request;
    type Response = B::Response;
    type Error = A::Error;

    type Config = A::Config;
    type Service = AndThenService<A::Service, B::Service>;
    type InitError = A::InitError;
    type Future = AndThenServiceFactoryResponse<A, B>;

    fn new_service(&self, cfg: A::Config) -> Self::Future {
        let inner = &*self.inner;
        AndThenServiceFactoryResponse::new(
            inner.0.new_service(cfg.clone()),
            inner.1.new_service(cfg),
        )
    }
}

impl<A, B> Clone for AndThenServiceFactory<A, B>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<
        Config = A::Config,
        Request = A::Response,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pin_project_lite::pin_project! {
    pub(crate) struct AndThenServiceFactoryResponse<A, B>
    where
        A: ServiceFactory,
        B: ServiceFactory<Request = A::Response>,
    {
        #[pin]
        fut_a: A::Future,
        #[pin]
        fut_b: B::Future,

        a: Option<A::Service>,
        b: Option<B::Service>,
    }
}

impl<A, B> AndThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<Request = A::Response>,
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

impl<A, B> Future for AndThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<Request = A::Response, Error = A::Error, InitError = A::InitError>,
{
    type Output = Result<AndThenService<A::Service, B::Service>, A::InitError>;

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
            Poll::Ready(Ok(AndThenService::new(
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

    impl Service for Srv1 {
        type Request = &'static str;
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

    impl Service for Srv2 {
        type Request = &'static str;
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

        let res = lazy(|cx| srv.poll_shutdown(cx, false)).await;
        assert_eq!(res, Poll::Ready(()));
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
