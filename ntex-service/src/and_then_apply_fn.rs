use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::cell::Cell;
use crate::{Service, ServiceFactory};

/// `Apply` service combinator
pub(crate) struct AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    srv: Cell<(A, B, F)>,
    r: PhantomData<(Fut, Res, Err)>,
}

impl<A, B, F, Fut, Res, Err> AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    /// Create new `Apply` combinator
    pub(crate) fn new(a: A, b: B, f: F) -> Self {
        Self {
            srv: Cell::new((a, b, f)),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> Clone for AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    fn clone(&self) -> Self {
        AndThenApplyFn {
            srv: self.srv.clone(),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> Service for AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    type Request = A::Request;
    type Response = Res;
    type Error = Err;
    type Future = AndThenApplyFnFuture<A, B, F, Fut, Res, Err>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = self.srv.get_mut();
        let not_ready = inner.0.poll_ready(cx)?.is_pending();
        if inner.1.poll_ready(cx)?.is_pending() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(&mut self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let srv = self.srv.get_mut();

        if srv.0.poll_shutdown(cx, is_error).is_ready()
            && srv.1.poll_shutdown(cx, is_error).is_ready()
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn call(&mut self, req: A::Request) -> Self::Future {
        let fut = self.srv.get_mut().0.call(req);
        AndThenApplyFnFuture {
            state: State::A(fut, Some(self.srv.clone())),
        }
    }
}

#[pin_project::pin_project]
pub(crate) struct AndThenApplyFnFuture<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error>,
    Err: From<B::Error>,
{
    #[pin]
    state: State<A, B, F, Fut, Res, Err>,
}

#[pin_project::pin_project]
enum State<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error>,
    Err: From<B::Error>,
{
    A(#[pin] A::Future, Option<Cell<(A, B, F)>>),
    B(#[pin] Fut),
    Empty,
}

impl<A, B, F, Fut, Res, Err> Future for AndThenApplyFnFuture<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    type Output = Result<Res, Err>;

    #[pin_project::project]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        #[project]
        match this.state.as_mut().project() {
            State::A(fut, b) => match fut.poll(cx)? {
                Poll::Ready(res) => {
                    let mut b = b.take().unwrap();
                    this.state.set(State::Empty);
                    let b = b.get_mut();
                    let fut = (&mut b.2)(res, &mut b.1);
                    this.state.set(State::B(fut));
                    self.poll(cx)
                }
                Poll::Pending => Poll::Pending,
            },
            State::B(fut) => fut.poll(cx).map(|r| {
                this.state.set(State::Empty);
                r
            }),
            State::Empty => {
                panic!("future must not be polled after it returned `Poll::Ready`")
            }
        }
    }
}

/// `AndThenApplyFn` service factory
pub(crate) struct AndThenApplyFnFactory<A, B, F, Fut, Res, Err> {
    srv: Rc<(A, B, F)>,
    r: PhantomData<(Fut, Res, Err)>,
}

impl<A, B, F, Fut, Res, Err> AndThenApplyFnFactory<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub(crate) fn new(a: A, b: B, f: F) -> Self {
        Self {
            srv: Rc::new((a, b, f)),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> Clone for AndThenApplyFnFactory<A, B, F, Fut, Res, Err> {
    fn clone(&self) -> Self {
        Self {
            srv: self.srv.clone(),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> ServiceFactory
    for AndThenApplyFnFactory<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    type Request = A::Request;
    type Response = Res;
    type Error = Err;
    type Service = AndThenApplyFn<A::Service, B::Service, F, Fut, Res, Err>;
    type Config = A::Config;
    type InitError = A::InitError;
    type Future = AndThenApplyFnFactoryResponse<A, B, F, Fut, Res, Err>;

    fn new_service(&self, cfg: A::Config) -> Self::Future {
        let srv = &*self.srv;
        AndThenApplyFnFactoryResponse {
            a: None,
            b: None,
            f: srv.2.clone(),
            fut_a: srv.0.new_service(cfg.clone()),
            fut_b: srv.1.new_service(cfg),
        }
    }
}

#[pin_project::pin_project]
pub(crate) struct AndThenApplyFnFactoryResponse<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error>,
    Err: From<B::Error>,
{
    #[pin]
    fut_b: B::Future,
    #[pin]
    fut_a: A::Future,
    f: F,
    a: Option<A::Service>,
    b: Option<B::Service>,
}

impl<A, B, F, Fut, Res, Err> Future
    for AndThenApplyFnFactoryResponse<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Fut + Clone,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    type Output =
        Result<AndThenApplyFn<A::Service, B::Service, F, Fut, Res, Err>, A::InitError>;

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
            Poll::Ready(Ok(AndThenApplyFn {
                srv: Cell::new((
                    this.a.take().unwrap(),
                    this.b.take().unwrap(),
                    this.f.clone(),
                )),
                r: PhantomData,
            }))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::future::{lazy, ok, Ready, TryFutureExt};

    use crate::{fn_service, pipeline, pipeline_factory, Service, ServiceFactory};

    #[derive(Clone)]
    struct Srv;
    impl Service for Srv {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = Ready<Result<(), ()>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: Self::Request) -> Self::Future {
            ok(req)
        }
    }

    #[ntex_rt::test]
    async fn test_service() {
        let mut srv = pipeline(|r: &'static str| ok(r))
            .and_then_apply_fn(Srv, |req: &'static str, s| {
                s.call(()).map_ok(move |res| (req, res))
            });
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }

    #[ntex_rt::test]
    async fn test_service_factory() {
        let new_srv =
            pipeline_factory(|| ok::<_, ()>(fn_service(|r: &'static str| ok(r))))
                .and_then_apply_fn(
                    || ok(Srv),
                    |req: &'static str, s| s.call(()).map_ok(move |res| (req, res)),
                );
        let mut srv = new_srv.new_service(()).await.unwrap();
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }
}
