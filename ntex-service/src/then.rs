use std::{future::Future, pin::Pin, task::Context, task::Poll};

use super::{Service, ServiceCall, ServiceCtx, ServiceFactory};

#[derive(Debug, Clone)]
/// Service for the `then` combinator, chaining a computation onto the end of
/// another service.
///
/// This is created by the `Pipeline::then` method.
pub struct Then<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> Then<A, B> {
    /// Create new `.then()` combinator
    pub(crate) fn new(svc1: A, svc2: B) -> Then<A, B> {
        Self { svc1, svc2 }
    }
}

impl<A, B, R> Service<R> for Then<A, B>
where
    A: Service<R>,
    B: Service<Result<A::Response, A::Error>, Error = A::Error>,
{
    type Response = B::Response;
    type Error = B::Error;
    type Future<'f> = ThenServiceResponse<'f, A, B, R> where Self: 'f, R: 'f;

    #[inline]
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
    fn call<'a>(&'a self, req: R, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        ThenServiceResponse {
            slf: self,
            state: State::A {
                fut: ctx.call(&self.svc1, req),
                ctx,
            },
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ThenServiceResponse<'f, A, B, R>
    where
        A: Service<R>,
        B: Service<Result<A::Response, A::Error>>,
    {
        slf: &'f Then<A, B>,
        #[pin]
        state: State<'f, A, B, R>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<'f, A, B, R>
    where
        A: Service<R>,
        A: 'f,
        A::Response: 'f,
        B: Service<Result<A::Response, A::Error>>,
        B: 'f,
        R: 'f,
    {
        A { #[pin] fut: ServiceCall<'f, A, R>, ctx: ServiceCtx<'f, Then<A, B>> },
        B { #[pin] fut: ServiceCall<'f, B, Result<A::Response, A::Error>> },
        Empty,
    }
}

impl<'a, A, B, R> Future for ThenServiceResponse<'a, A, B, R>
where
    A: Service<R>,
    B: Service<Result<A::Response, A::Error>>,
{
    type Output = Result<B::Response, B::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        match this.state.as_mut().project() {
            StateProject::A { fut, ctx } => match fut.poll(cx) {
                Poll::Ready(res) => {
                    let fut = ctx.call(&this.slf.svc2, res);
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

#[derive(Debug, Clone)]
/// `.then()` service factory combinator
pub struct ThenFactory<A, B> {
    svc1: A,
    svc2: B,
}

impl<A, B> ThenFactory<A, B> {
    /// Create new factory for `Then` combinator
    pub(crate) fn new(svc1: A, svc2: B) -> Self {
        Self { svc1, svc2 }
    }
}

impl<A, B, R, C> ServiceFactory<R, C> for ThenFactory<A, B>
where
    A: ServiceFactory<R, C>,
    B: ServiceFactory<
        Result<A::Response, A::Error>,
        C,
        Error = A::Error,
        InitError = A::InitError,
    >,
    C: Clone,
{
    type Response = B::Response;
    type Error = A::Error;

    type Service = Then<A::Service, B::Service>;
    type InitError = A::InitError;
    type Future<'f> = ThenFactoryResponse<'f, A, B, R, C> where Self: 'f, C: 'f;

    fn create(&self, cfg: C) -> Self::Future<'_> {
        ThenFactoryResponse {
            fut_a: self.svc1.create(cfg.clone()),
            fut_b: self.svc2.create(cfg),
            a: None,
            b: None,
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct ThenFactoryResponse<'f, A, B, R, C>
    where
        A: ServiceFactory<R, C>,
        B: ServiceFactory<Result<A::Response, A::Error>, C,
           Error = A::Error,
           InitError = A::InitError,
        >,
        A: 'f,
        B: 'f,
        C: 'f,
    {
        #[pin]
        fut_b: B::Future<'f>,
        #[pin]
        fut_a: A::Future<'f>,
        a: Option<A::Service>,
        b: Option<B::Service>,
    }
}

impl<'f, A, B, R, C> Future for ThenFactoryResponse<'f, A, B, R, C>
where
    A: ServiceFactory<R, C>,
    B: ServiceFactory<
        Result<A::Response, A::Error>,
        C,
        Error = A::Error,
        InitError = A::InitError,
    >,
{
    type Output = Result<Then<A::Service, B::Service>, A::InitError>;

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
            Poll::Ready(Ok(Then::new(
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
    use ntex_util::future::{lazy, Ready};
    use std::{cell::Cell, rc::Rc, task::Context, task::Poll};

    use crate::{chain, chain_factory, Service, ServiceCtx};

    #[derive(Clone)]
    struct Srv1(Rc<Cell<usize>>);

    impl Service<Result<&'static str, &'static str>> for Srv1 {
        type Response = &'static str;
        type Error = ();
        type Future<'f> = Ready<Self::Response, Self::Error>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call<'a>(
            &'a self,
            req: Result<&'static str, &'static str>,
            _: ServiceCtx<'a, Self>,
        ) -> Self::Future<'a> {
            match req {
                Ok(msg) => Ready::Ok(msg),
                Err(_) => Ready::Err(()),
            }
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>);

    impl Service<Result<&'static str, ()>> for Srv2 {
        type Response = (&'static str, &'static str);
        type Error = ();
        type Future<'f> = Ready<Self::Response, ()>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        fn call<'a>(
            &'a self,
            req: Result<&'static str, ()>,
            _: ServiceCtx<'a, Self>,
        ) -> Self::Future<'a> {
            match req {
                Ok(msg) => Ready::Ok((msg, "ok")),
                Err(()) => Ready::Ok(("srv2", "err")),
            }
        }
    }

    #[ntex::test]
    async fn test_poll_ready() {
        let cnt = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone())).then(Srv2(cnt.clone()));
        let res = lazy(|cx| srv.poll_ready(cx)).await;
        assert_eq!(res, Poll::Ready(Ok(())));
        assert_eq!(cnt.get(), 2);
        let res = lazy(|cx| srv.poll_shutdown(cx)).await;
        assert_eq!(res, Poll::Ready(()));
    }

    #[ntex::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone())).then(Srv2(cnt)).clone().pipeline();

        let res = srv.call(Ok("srv1")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "ok"));

        let res = srv.call(Err("srv")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv2", "err"));
    }

    #[ntex::test]
    async fn test_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let blank = move || Ready::<_, ()>::Ok(Srv1(cnt2.clone()));
        let factory = chain_factory(blank)
            .then(move || Ready::Ok(Srv2(cnt.clone())))
            .clone();
        let srv = factory.pipeline(&()).await.unwrap();
        let res = srv.call(Ok("srv1")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "ok"));

        let res = srv.call(Err("srv")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv2", "err"));
    }
}
