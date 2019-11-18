use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::{Service, ServiceFactory};
use crate::cell::Cell;

/// Service for the `and_then` combinator, chaining a computation onto the end
/// of another service which completes successfully.
///
/// This is created by the `ServiceExt::and_then` method.
pub struct AndThenService<A, B> {
    a: A,
    b: Cell<B>,
}

impl<A, B> AndThenService<A, B> {
    /// Create new `AndThen` combinator
    pub(crate) fn new(a: A, b: B) -> Self
    where
        A: Service,
        B: Service<Request = A::Response, Error = A::Error>,
    {
        Self { a, b: Cell::new(b) }
    }
}

impl<A, B> Clone for AndThenService<A, B>
where
    A: Clone,
{
    fn clone(&self) -> Self {
        AndThenService {
            a: self.a.clone(),
            b: self.b.clone(),
        }
    }
}

impl<A, B> Service for AndThenService<A, B>
where
    A: Service,
    B: Service<Request = A::Response, Error = A::Error>,
    A::Future: Unpin,
    B::Future: Unpin,
{
    type Request = A::Request;
    type Response = B::Response;
    type Error = A::Error;
    type Future = AndThenServiceResponse<A, B>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let not_ready = !self.a.poll_ready(cx)?.is_ready();
        if !self.b.get_mut().poll_ready(cx)?.is_ready() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn call(&mut self, req: A::Request) -> Self::Future {
        AndThenServiceResponse::new(self.a.call(req), self.b.clone())
    }
}

pub struct AndThenServiceResponse<A, B>
where
    A: Service,
    B: Service<Request = A::Response, Error = A::Error>,
{
    b: Cell<B>,
    fut_b: Option<B::Future>,
    fut_a: Option<A::Future>,
}

impl<A, B> AndThenServiceResponse<A, B>
where
    A: Service,
    B: Service<Request = A::Response, Error = A::Error>,
    A::Future: Unpin,
    B::Future: Unpin,
{
    fn new(a: A::Future, b: Cell<B>) -> Self {
        AndThenServiceResponse {
            b,
            fut_a: Some(a),
            fut_b: None,
        }
    }
}

impl<A, B> Future for AndThenServiceResponse<A, B>
where
    A: Service,
    B: Service<Request = A::Response, Error = A::Error>,
    A::Future: Unpin,
    B::Future: Unpin,
{
    type Output = Result<B::Response, A::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.get_mut();

        loop {
            if let Some(ref mut fut) = this.fut_b {
                return Pin::new(fut).poll(cx);
            }

            match Pin::new(&mut this.fut_a.as_mut().expect("Bug in actix-service")).poll(cx) {
                Poll::Ready(Ok(resp)) => {
                    let _ = this.fut_a.take();
                    this.fut_b = Some(this.b.get_mut().call(resp));
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// `.and_then()` service factory combinator
pub struct AndThenServiceFactory<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory,
{
    a: A,
    b: B,
}

impl<A, B> AndThenServiceFactory<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<
        Config = A::Config,
        Request = A::Response,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    /// Create new `AndThenFactory` combinator
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A, B> ServiceFactory for AndThenServiceFactory<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<
        Config = A::Config,
        Request = A::Response,
        Error = A::Error,
        InitError = A::InitError,
    >,
    A::Future: Unpin,
    <A::Service as Service>::Future: Unpin,
    B::Future: Unpin,
    <B::Service as Service>::Future: Unpin,
{
    type Request = A::Request;
    type Response = B::Response;
    type Error = A::Error;

    type Config = A::Config;
    type Service = AndThenService<A::Service, B::Service>;
    type InitError = A::InitError;
    type Future = AndThenServiceFactoryResponse<A, B>;

    fn new_service(&self, cfg: &A::Config) -> Self::Future {
        AndThenServiceFactoryResponse::new(self.a.new_service(cfg), self.b.new_service(cfg))
    }
}

impl<A, B> Clone for AndThenServiceFactory<A, B>
where
    A: ServiceFactory + Clone,
    B: ServiceFactory + Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
        }
    }
}

pub struct AndThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<Request = A::Response>,
{
    fut_b: B::Future,
    fut_a: A::Future,

    a: Option<A::Service>,
    b: Option<B::Service>,
}

impl<A, B> AndThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<Request = A::Response>,
    A::Future: Unpin,
    <A::Service as Service>::Future: Unpin,
    B::Future: Unpin,
    <B::Service as Service>::Future: Unpin,
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

impl<A, B> Unpin for AndThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<Request = A::Response, Error = A::Error, InitError = A::InitError>,
    A::Future: Unpin,
    <A::Service as Service>::Future: Unpin,
    B::Future: Unpin,
    <B::Service as Service>::Future: Unpin,
{
}

impl<A, B> Future for AndThenServiceFactoryResponse<A, B>
where
    A: ServiceFactory,
    B: ServiceFactory<Request = A::Response, Error = A::Error, InitError = A::InitError>,
    A::Future: Unpin,
    <A::Service as Service>::Future: Unpin,
    B::Future: Unpin,
    <B::Service as Service>::Future: Unpin,
{
    type Output = Result<AndThenService<A::Service, B::Service>, A::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.a.is_none() {
            if let Poll::Ready(service) = Pin::new(&mut this.fut_a).poll(cx)? {
                this.a = Some(service);
            }
        }
        if this.b.is_none() {
            if let Poll::Ready(service) = Pin::new(&mut this.fut_b).poll(cx)? {
                this.b = Some(service);
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
    use std::cell::Cell;
    use std::rc::Rc;
    use std::task::{Context, Poll};

    use futures::future::{lazy, ok, ready, Ready};

    use crate::{factory_fn, pipeline, pipeline_factory, Service, ServiceFactory};

    struct Srv1(Rc<Cell<usize>>);

    impl Service for Srv1 {
        type Request = &'static str;
        type Response = &'static str;
        type Error = ();
        type Future = Ready<Result<Self::Response, ()>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: &'static str) -> Self::Future {
            ok(req)
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>);

    impl Service for Srv2 {
        type Request = &'static str;
        type Response = (&'static str, &'static str);
        type Error = ();
        type Future = Ready<Result<Self::Response, ()>>;

        fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: &'static str) -> Self::Future {
            ok((req, "srv2"))
        }
    }

    #[tokio::test]
    async fn test_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let mut srv = pipeline(Srv1(cnt.clone())).and_then(Srv2(cnt.clone()));
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 2);
    }

    #[tokio::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let mut srv = pipeline(Srv1(cnt.clone())).and_then(Srv2(cnt));
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), (("srv1", "srv2")));
    }

    #[tokio::test]
    async fn test_new_service() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let new_srv =
            pipeline_factory(factory_fn(move || ready(Ok::<_, ()>(Srv1(cnt2.clone())))))
                .and_then(move || ready(Ok(Srv2(cnt.clone()))));

        let mut srv = new_srv.new_service(&()).await.unwrap();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }
}
