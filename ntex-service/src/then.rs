use super::{util, Service, ServiceCtx, ServiceFactory};

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
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        util::ready(&self.svc1, &self.svc2, ctx).await
    }

    #[inline]
    fn poll(&self, cx: &mut std::task::Context<'_>) -> Result<(), Self::Error> {
        self.svc1.poll(cx)?;
        self.svc2.poll(cx)
    }

    #[inline]
    async fn shutdown(&self) {
        util::shutdown(&self.svc1, &self.svc2).await
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
    use ntex_util::future::lazy;
    use std::{cell::Cell, rc::Rc, task::Context};

    use crate::{chain, chain_factory, fn_factory, Service, ServiceCtx};

    #[derive(Clone)]
    struct Srv1(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<Result<&'static str, &'static str>> for Srv1 {
        type Response = &'static str;
        type Error = ();

        async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        fn poll(&self, _: &mut Context<'_>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
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

        async fn shutdown(&self) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<Result<&'static str, ()>> for Srv2 {
        type Response = (&'static str, &'static str);
        type Error = ();

        async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        fn poll(&self, _: &mut Context<'_>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
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

        async fn shutdown(&self) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone(), cnt_sht.clone()))
            .then(Srv2(cnt.clone(), cnt_sht.clone()))
            .into_pipeline();
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
        assert_eq!(cnt.get(), 2);

        lazy(|cx| srv.clone().poll(cx)).await.unwrap();
        assert_eq!(cnt.get(), 4);

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone(), Rc::new(Cell::new(0))))
            .then(Srv2(cnt, Rc::new(Cell::new(0))))
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
        let blank = fn_factory(move || {
            let cnt = cnt2.clone();
            async move { Ok::<_, ()>(Srv1(cnt, Rc::new(Cell::new(0)))) }
        });
        let factory = chain_factory(blank)
            .then(fn_factory(move || {
                let cnt = cnt.clone();
                async move { Ok(Srv2(cnt.clone(), Rc::new(Cell::new(0)))) }
            }))
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
