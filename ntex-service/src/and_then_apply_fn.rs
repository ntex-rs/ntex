use std::{
    future::Future, marker::PhantomData, pin::Pin, rc::Rc, task::Context, task::Poll,
};

use crate::{Service, ServiceFactory};

/// `Apply` service combinator
pub(crate) struct AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: Fn(A::Response, &B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    srv: Rc<(A, B, F)>,
    r: PhantomData<(Fut, Res, Err)>,
}

impl<A, B, F, Fut, Res, Err> AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: Fn(A::Response, &B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    /// Create new `Apply` combinator
    pub(crate) fn new(a: A, b: B, f: F) -> Self {
        Self {
            srv: Rc::new((a, b, f)),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Fut, Res, Err> Clone for AndThenApplyFn<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: Fn(A::Response, &B) -> Fut,
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
    F: Fn(A::Response, &B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    type Request = A::Request;
    type Response = Res;
    type Error = Err;
    type Future = AndThenApplyFnFuture<A, B, F, Fut, Res, Err>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = self.srv.as_ref();
        let not_ready = inner.0.poll_ready(cx)?.is_pending();
        if inner.1.poll_ready(cx)?.is_pending() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        let srv = self.srv.as_ref();

        if srv.0.poll_shutdown(cx, is_error).is_ready()
            && srv.1.poll_shutdown(cx, is_error).is_ready()
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn call(&self, req: A::Request) -> Self::Future {
        let fut = self.srv.as_ref().0.call(req);
        AndThenApplyFnFuture {
            state: State::A {
                fut,
                b: Some(self.srv.clone()),
            },
        }
    }
}

pin_project_lite::pin_project! {
    pub(crate) struct AndThenApplyFnFuture<A, B, F, Fut, Res, Err>
    where
        A: Service,
        B: Service,
        F: Fn(A::Response, &B) -> Fut,
        Fut: Future<Output = Result<Res, Err>>,
        Err: From<A::Error>,
        Err: From<B::Error>,
    {
        #[pin]
        state: State<A, B, F, Fut, Res, Err>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<A, B, F, Fut, Res, Err>
    where
        A: Service,
        B: Service,
        F: Fn(A::Response, &B) -> Fut,
        Fut: Future<Output = Result<Res, Err>>,
        Err: From<A::Error>,
        Err: From<B::Error>,
    {
        A { #[pin] fut: A::Future, b: Option<Rc<(A, B, F)>> },
        B { #[pin] fut: Fut },
        Empty,
    }
}

impl<A, B, F, Fut, Res, Err> Future for AndThenApplyFnFuture<A, B, F, Fut, Res, Err>
where
    A: Service,
    B: Service,
    F: Fn(A::Response, &B) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
    Err: From<A::Error> + From<B::Error>,
{
    type Output = Result<Res, Err>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            StateProject::A { fut, b } => match fut.poll(cx)? {
                Poll::Ready(res) => {
                    let b = b.take().unwrap();
                    this.state.set(State::Empty);
                    let b = b.as_ref();
                    let fut = (&b.2)(res, &b.1);
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

/// `AndThenApplyFn` service factory
pub(crate) struct AndThenApplyFnFactory<A, B, F, Fut, Res, Err> {
    srv: Rc<(A, B, F)>,
    r: PhantomData<(Fut, Res, Err)>,
}

impl<A, B, F, Fut, Res, Err> AndThenApplyFnFactory<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: Fn(A::Response, &B::Service) -> Fut + Clone,
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
    F: Fn(A::Response, &B::Service) -> Fut + Clone,
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

pin_project_lite::pin_project! {
    pub(crate) struct AndThenApplyFnFactoryResponse<A, B, F, Fut, Res, Err>
    where
        A: ServiceFactory,
        B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
        F: Fn(A::Response, &B::Service) -> Fut,
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
}

impl<A, B, F, Fut, Res, Err> Future
    for AndThenApplyFnFactoryResponse<A, B, F, Fut, Res, Err>
where
    A: ServiceFactory,
    B: ServiceFactory<Config = A::Config, InitError = A::InitError>,
    F: Fn(A::Response, &B::Service) -> Fut + Clone,
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
                srv: Rc::new((
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

    use crate::{fn_service, pipeline, pipeline_factory, Service, ServiceFactory};
    use ntex_util::future::{lazy, Ready};

    #[derive(Clone)]
    struct Srv;
    impl Service for Srv {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = Ready<(), ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&self, _: Self::Request) -> Self::Future {
            Ready::Ok(())
        }
    }

    #[ntex::test]
    async fn test_service() {
        let srv = pipeline(Ready::<_, ()>::Ok)
            .and_then_apply_fn(Srv, |req: &'static str, s| {
                let f = s.call(());
                async move { f.await.map(move |res| (req, res)) }
            })
            .clone();
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));

        let res = lazy(|cx| srv.poll_shutdown(cx, false)).await;
        assert_eq!(res, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_service_factory() {
        let new_srv =
            pipeline_factory(|| Ready::<_, ()>::Ok(fn_service(Ready::<_, ()>::Ok)))
                .and_then_apply_fn(
                    || Ready::Ok(Srv),
                    |req: &'static str, s| {
                        let f = s.call(());
                        async move { f.await.map(move |res| (req, res)) }
                    },
                )
                .clone();
        let srv = new_srv.new_service(()).await.unwrap();
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }
}
