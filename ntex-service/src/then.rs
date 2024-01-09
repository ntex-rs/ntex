use std::{task::Context, task::Poll};

use super::{Service, ServiceCtx, ServiceFactory};

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
    async fn call(
        &self,
        req: R,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.svc2, ctx.call(&self.svc1, req).await).await
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

    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        Ok(Then {
            svc1: self.svc1.create(cfg.clone()).await?,
            svc2: self.svc2.create(cfg).await?,
        })
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

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        async fn call(
            &self,
            req: Result<&'static str, &'static str>,
            _: ServiceCtx<'_, Self>,
        ) -> Result<&'static str, ()> {
            match req {
                Ok(msg) => Ok(msg),
                Err(_) => Err(()),
            }
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>);

    impl Service<Result<&'static str, ()>> for Srv2 {
        type Response = (&'static str, &'static str);
        type Error = ();

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.set(self.0.get() + 1);
            Poll::Ready(Ok(()))
        }

        async fn call(
            &self,
            req: Result<&'static str, ()>,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, ()> {
            match req {
                Ok(msg) => Ok((msg, "ok")),
                Err(()) => Ok(("srv2", "err")),
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
        let srv = chain(Srv1(cnt.clone()))
            .then(Srv2(cnt))
            .clone()
            .into_pipeline();

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
