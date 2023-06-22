use std::{future::Future, pin::Pin, task::Context, task::Poll};

use super::{Service, ServiceCall, ServiceCtx, ServiceFactory};

/// Service for the `and_then` combinator, chaining a computation onto the end
/// of another service which completes successfully.
///
/// This is created by the `ServiceExt::and_then` method.
pub struct AndThen<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> AndThen<A, B> {
    /// Create new `AndThen` combinator
    pub(crate) fn new(svc1: A, svc2: B) -> Self {
        Self { svc1, svc2 }
    }
}

impl<A, B> Clone for AndThen<A, B>
where
    A: Clone,
    B: Clone,
{
    fn clone(&self) -> Self {
        AndThen {
            svc1: self.svc1.clone(),
            svc2: self.svc2.clone(),
        }
    }
}

impl<A, B, Req> Service<Req> for AndThen<A, B>
where
    A: Service<Req>,
    B: Service<A::Response, Error = A::Error>,
{
    type Response = B::Response;
    type Error = A::Error;
    type Future<'f> = AndThenServiceResponse<'f, A, B, Req> where Self: 'f, Req: 'f;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let not_ready = !self.svc1.poll_ready(cx)?.is_ready();
        if !self.svc2.poll_ready(cx)?.is_ready() || not_ready {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.svc1.poll_shutdown(cx).is_ready() && self.svc2.poll_shutdown(cx).is_ready()
        {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    #[inline]
    fn call<'a>(&'a self, req: Req, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        AndThenServiceResponse {
            slf: self,
            state: State::A {
                fut: ctx.call(&self.svc1, req),
                ctx: Some(ctx),
            },
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct AndThenServiceResponse<'f, A, B, Req>
    where
        A: Service<Req>,
        B: Service<A::Response, Error = A::Error>,
    {
        slf: &'f AndThen<A, B>,
        #[pin]
        state: State<'f, A, B, Req>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<'f, A, B, Req>
    where
        A: Service<Req>,
        A: 'f,
        Req: 'f,
        B: Service<A::Response, Error = A::Error>,
        B: 'f,
    {
        A { #[pin] fut: ServiceCall<'f, A, Req>, ctx: Option<ServiceCtx<'f, AndThen<A, B>>> },
        B { #[pin] fut: ServiceCall<'f, B, A::Response> },
        Empty,
    }
}

impl<'f, A, B, Req> Future for AndThenServiceResponse<'f, A, B, Req>
where
    A: Service<Req>,
    B: Service<A::Response, Error = A::Error>,
{
    type Output = Result<B::Response, A::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            StateProject::A { fut, ctx } => match fut.poll(cx)? {
                Poll::Ready(res) => {
                    let fut = ctx.take().unwrap().call(&this.slf.svc2, res);
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
pub struct AndThenFactory<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> AndThenFactory<A, B> {
    /// Create new `AndThenFactory` combinator
    pub fn new(svc1: A, svc2: B) -> Self {
        Self { svc1, svc2 }
    }
}

impl<A, B, Req, Cfg> ServiceFactory<Req, Cfg> for AndThenFactory<A, B>
where
    A: ServiceFactory<Req, Cfg>,
    B: ServiceFactory<A::Response, Cfg, Error = A::Error, InitError = A::InitError>,
    Cfg: Clone,
{
    type Response = B::Response;
    type Error = A::Error;

    type Service = AndThen<A::Service, B::Service>;
    type InitError = A::InitError;
    type Future<'f> = AndThenFactoryResponse<'f, A, B, Req, Cfg> where Self: 'f, Cfg: 'f;

    #[inline]
    fn create(&self, cfg: Cfg) -> Self::Future<'_> {
        AndThenFactoryResponse {
            fut1: self.svc1.create(cfg.clone()),
            fut2: self.svc2.create(cfg),
            svc1: None,
            svc2: None,
        }
    }
}

impl<A, B> Clone for AndThenFactory<A, B>
where
    A: Clone,
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            svc1: self.svc1.clone(),
            svc2: self.svc2.clone(),
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct AndThenFactoryResponse<'f, A, B, Req, Cfg>
    where
        A: ServiceFactory<Req, Cfg>,
        A: 'f,
        B: ServiceFactory<A::Response, Cfg>,
        B: 'f,
        Cfg: 'f
    {
        #[pin]
        fut1: A::Future<'f>,
        #[pin]
        fut2: B::Future<'f>,

        svc1: Option<A::Service>,
        svc2: Option<B::Service>,
    }
}

impl<'f, A, B, Req, Cfg> Future for AndThenFactoryResponse<'f, A, B, Req, Cfg>
where
    A: ServiceFactory<Req, Cfg>,
    B: ServiceFactory<A::Response, Cfg, Error = A::Error, InitError = A::InitError>,
{
    type Output = Result<AndThen<A::Service, B::Service>, A::InitError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if this.svc1.is_none() {
            if let Poll::Ready(service) = this.fut1.poll(cx)? {
                *this.svc1 = Some(service);
            }
        }
        if this.svc2.is_none() {
            if let Poll::Ready(service) = this.fut2.poll(cx)? {
                *this.svc2 = Some(service);
            }
        }
        if this.svc1.is_some() && this.svc2.is_some() {
            Poll::Ready(Ok(AndThen::new(
                this.svc1.take().unwrap(),
                this.svc2.take().unwrap(),
            )))
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, task::Context, task::Poll};

    use crate::{chain, chain_factory, fn_factory, Service, ServiceCtx};
    use ntex_util::future::{lazy, Ready};

    #[derive(Clone)]
    struct Srv1(Rc<Cell<usize>>);

    impl Service<&'static str> for Srv1 {
        type Response = &'static str;
        type Error = ();
        type Future<'f> = Ready<Self::Response, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call<'a>(
            &'a self,
            req: &'static str,
            _: ServiceCtx<'a, Self>,
        ) -> Self::Future<'a> {
            Ready::Ok(req)
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>);

    impl Service<&'static str> for Srv2 {
        type Response = (&'static str, &'static str);
        type Error = ();
        type Future<'f> = Ready<Self::Response, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call<'a>(
            &'a self,
            req: &'static str,
            _: ServiceCtx<'a, Self>,
        ) -> Self::Future<'a> {
            Ready::Ok((req, "srv2"))
        }
    }

    #[ntex::test]
    async fn test_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone())).and_then(Srv2(cnt.clone())).clone();
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 2);
        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone())).and_then(Srv2(cnt)).pipeline();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }

    #[ntex::test]
    async fn test_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let new_srv = chain_factory(fn_factory(move || {
            Ready::from(Ok::<_, ()>(Srv1(cnt2.clone())))
        }))
        .and_then(move || Ready::from(Ok(Srv2(cnt.clone()))))
        .clone();

        let srv = new_srv.pipeline(&()).await.unwrap();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }
}
