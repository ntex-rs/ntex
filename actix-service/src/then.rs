use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::{Service, ServiceFactory};
use crate::cell::Cell;

/// Service for the `then` combinator, chaining a computation onto the end of
/// another service.
///
/// This is created by the `ServiceExt::then` method.
pub struct ThenService<A, B> {
    a: A,
    b: Cell<B>,
}

impl<A, B> ThenService<A, B> {
    /// Create new `.then()` combinator
    pub(crate) fn new(a: A, b: B) -> ThenService<A, B>
    where
        A: Service,
        B: Service<Request = Result<A::Response, A::Error>, Error = A::Error>,
    {
        Self { a, b: Cell::new(b) }
    }
}

impl<A, B> Clone for ThenService<A, B>
where
    A: Clone,
{
    fn clone(&self) -> Self {
        ThenService {
            a: self.a.clone(),
            b: self.b.clone(),
        }
    }
}

impl<A, B> Service for ThenService<A, B>
where
    A: Service,
    B: Service<Request = Result<A::Response, A::Error>, Error = A::Error>,
{
    type Request = A::Request;
    type Response = B::Response;
    type Error = B::Error;
    type Future = ThenServiceResponse<A, B>;

    fn poll_ready(&mut self, ctx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let not_ready = !self.a.poll_ready(ctx)?.is_ready();
        if !self.b.get_mut().poll_ready(ctx)?.is_ready() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn call(&mut self, req: A::Request) -> Self::Future {
        ThenServiceResponse {
            state: State::A(self.a.call(req), Some(self.b.clone())),
        }
    }
}

#[pin_project::pin_project]
pub struct ThenServiceResponse<A, B>
where
    A: Service,
    B: Service<Request = Result<A::Response, A::Error>>,
{
    #[pin]
    state: State<A, B>,
}

#[pin_project::pin_project]
enum State<A, B>
where
    A: Service,
    B: Service<Request = Result<A::Response, A::Error>>,
{
    A(#[pin] A::Future, Option<Cell<B>>),
    B(#[pin] B::Future),
    Empty,
}

impl<A, B> Future for ThenServiceResponse<A, B>
where
    A: Service,
    B: Service<Request = Result<A::Response, A::Error>>,
{
    type Output = Result<B::Response, B::Error>;

    #[pin_project::project]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        #[project]
        match this.state.as_mut().project() {
            State::A(fut, b) => match fut.poll(cx) {
                Poll::Ready(res) => {
                    let mut b = b.take().unwrap();
                    this.state.set(State::Empty); // drop fut A
                    let fut = b.get_mut().call(res);
                    this.state.set(State::B(fut));
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            State::B(fut) => fut.poll(cx).map(|r| {
                this.state.set(State::Empty);
                r
            }),
            State::Empty => panic!("future must not be polled after it returned `Poll::Ready`"),
        }
    }
}

/// `.then()` service factory combinator
pub struct ThenServiceFactory<A, B> {
    a: A,
    b: B,
}

impl<A, B> ThenServiceFactory<A, B>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<
        Config = A::Config,
        Request = Result<A::Response, A::Error>,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    /// Create new `AndThen` combinator
    pub(crate) fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B> ServiceFactory for ThenServiceFactory<A, B>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<
        Config = A::Config,
        Request = Result<A::Response, A::Error>,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    type Request = A::Request;
    type Response = B::Response;
    type Error = A::Error;

    type Config = A::Config;
    type Service = ThenService<A::Service, B::Service>;
    type InitError = A::InitError;
    type Future = ThenServiceFactoryResponse<A, B>;

    fn new_service(&self, cfg: A::Config) -> Self::Future {
        ThenServiceFactoryResponse::new(
            self.a.new_service(cfg.clone()),
            self.b.new_service(cfg),
        )
    }
}

impl<A, B> Clone for ThenServiceFactory<A, B>
where
    A: Clone,
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
        }
    }
}

#[pin_project::pin_project]
pub struct ThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<
        Config = A::Config,
        Request = Result<A::Response, A::Error>,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    #[pin]
    fut_b: B::Future,
    #[pin]
    fut_a: A::Future,
    a: Option<A::Service>,
    b: Option<B::Service>,
}

impl<A, B> ThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<
        Config = A::Config,
        Request = Result<A::Response, A::Error>,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    fn new(fut_a: A::Future, fut_b: B::Future) -> Self {
        Self {
            fut_a,
            fut_b,
            a: None,
            b: None,
        }
    }
}

impl<A, B> Future for ThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<
        Config = A::Config,
        Request = Result<A::Response, A::Error>,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    type Output = Result<ThenService<A::Service, B::Service>, A::InitError>;

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
            Poll::Ready(Ok(ThenService::new(
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
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    use futures_util::future::{err, lazy, ok, ready, Ready};

    use crate::{pipeline, pipeline_factory, Service, ServiceFactory};

    #[derive(Clone)]
    struct Srv1(Rc<Cell<usize>>);

    impl Service for Srv1 {
        type Request = Result<&'static str, &'static str>;
        type Response = &'static str;
        type Error = ();
        type Future = Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Result<&'static str, &'static str>) -> Self::Future {
            match req {
                Ok(msg) => ok(msg),
                Err(_) => err(()),
            }
        }
    }

    struct Srv2(Rc<Cell<usize>>);

    impl Service for Srv2 {
        type Request = Result<&'static str, ()>;
        type Response = (&'static str, &'static str);
        type Error = ();
        type Future = Ready<Result<Self::Response, ()>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Err(()))
        }

        fn call(&mut self, req: Result<&'static str, ()>) -> Self::Future {
            match req {
                Ok(msg) => ok((msg, "ok")),
                Err(()) => ok(("srv2", "err")),
            }
        }
    }

    #[actix_rt::test]
    async fn test_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let mut srv = pipeline(Srv1(cnt.clone())).then(Srv2(cnt.clone()));
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Err(())));
        assert_eq!(cnt.get(), 2);
    }

    #[actix_rt::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let mut srv = pipeline(Srv1(cnt.clone())).then(Srv2(cnt));

        let res = srv.call(Ok("srv1")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), (("srv1", "ok")));

        let res = srv.call(Err("srv")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), (("srv2", "err")));
    }

    #[actix_rt::test]
    async fn test_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let blank = move || ready(Ok::<_, ()>(Srv1(cnt2.clone())));
        let factory = pipeline_factory(blank).then(move || ready(Ok(Srv2(cnt.clone()))));
        let mut srv = factory.new_service(&()).await.unwrap();
        let res = srv.call(Ok("srv1")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), (("srv1", "ok")));

        let res = srv.call(Err("srv")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), (("srv2", "err")));
    }
}
