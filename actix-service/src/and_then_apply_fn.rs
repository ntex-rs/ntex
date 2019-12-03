use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::ready;

use crate::cell::Cell;
use crate::{Service, ServiceFactory};

/// `Apply` service combinator
pub struct AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    a: A,
    b: Cell<B>,
    f: Cell<F>,
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
            a,
            f: Cell::new(f),
            b: Cell::new(b),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> Clone for AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service + Clone,
    B: Service,
    F: FnMut(A::Response, &mut B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    fn clone(&self) -> Self {
        AndThenApplyFn {
            a: self.a.clone(),
            b: self.b.clone(),
            f: self.f.clone(),
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
        let not_ready = self.a.poll_ready(cx)?.is_pending();
        if self.b.get_mut().poll_ready(cx)?.is_pending() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn call(&mut self, req: A::Request) -> Self::Future {
        AndThenApplyFnFuture {
            b: self.b.clone(),
            f: self.f.clone(),
            fut_a: Some(self.a.call(req)),
            fut_b: None,
        }
    }
}

pin_project! {
    pub struct AndThenApplyFnFuture<A, B, F, Fut, Res, Err>
    where
        A: Service,
        B: Service,
        F: FnMut(A::Response, &mut B) -> Fut,
        Fut: Future<Output = Result<Res, Err>>,
        Err: From<A::Error>,
        Err: From<B::Error>,
    {
        b: Cell<B>,
        f: Cell<F>,
        #[pin]
        fut_a: Option<A::Future>,
        #[pin]
        fut_b: Option<Fut>,
    }
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

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        if let Some(fut) = this.fut_b.as_pin_mut() {
            return Poll::Ready(ready!(fut.poll(cx)).map_err(|e| e.into()));
        }

        match this
            .fut_a
            .as_pin_mut()
            .expect("Bug in actix-service")
            .poll(cx)
        {
            Poll::Ready(Ok(resp)) => {
                this = self.as_mut().project();
                this.fut_b
                    .set(Some((&mut *this.f.get_mut())(resp, this.b.get_mut())));
                this.fut_a.set(None);
                self.poll(cx)
            }
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(err)) => Poll::Ready(Err(err.into())),
        }
    }
}

/// `AndThenApplyFn` service factory
pub struct AndThenApplyFnFactory<A, B, F, Fut, Res, Err> {
    a: A,
    b: B,
    f: Cell<F>,
    r: PhantomData<(Fut, Res, Err)>,
}

impl<A, B, F, Fut, Res, Err> AndThenApplyFnFactory<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub(crate) fn new(a: A, b: B, f: F) -> Self {
        Self {
            a: a,
            b: b,
            f: Cell::new(f),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> Clone for AndThenApplyFnFactory<A, B, F, Fut, Res, Err>
where
    A: Clone,
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> ServiceFactory for AndThenApplyFnFactory<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    A::Config: Clone,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Fut,
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
        AndThenApplyFnFactoryResponse {
            a: None,
            b: None,
            f: self.f.clone(),
            fut_a: self.a.new_service(cfg.clone()),
            fut_b: self.b.new_service(cfg),
        }
    }
}

pin_project! {
    pub struct AndThenApplyFnFactoryResponse<A, B, F, Fut, Res, Err>
    where
        A: ServiceFactory,
        B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
        F: FnMut(A::Response, &mut B::Service) -> Fut,
        Fut: Future<Output = Result<Res, Err>>,
        Err: From<A::Error>,
        Err: From<B::Error>,
    {
        #[pin]
        fut_b: B::Future,
        #[pin]
        fut_a: A::Future,
        f: Cell<F>,
        a: Option<A::Service>,
        b: Option<B::Service>,
    }
}

impl<A, B, F, Fut, Res, Err> Future for AndThenApplyFnFactoryResponse<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Fut,
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
                f: this.f.clone(),
                a: this.a.take().unwrap(),
                b: Cell::new(this.b.take().unwrap()),
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

    use futures::future::{lazy, ok, Ready, TryFutureExt};

    use crate::{pipeline, pipeline_factory, service_fn2, Service, ServiceFactory};

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

    #[actix_rt::test]
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

    #[actix_rt::test]
    async fn test_service_factory() {
        let new_srv = pipeline_factory(|| ok::<_, ()>(service_fn2(|r: &'static str| ok(r))))
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
