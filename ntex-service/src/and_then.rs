use super::{util, Service, ServiceCtx, ServiceFactory};

#[derive(Clone, Debug)]
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

impl<A, B, Req> Service<Req> for AndThen<A, B>
where
    A: Service<Req>,
    B: Service<A::Response, Error = A::Error>,
{
    type Response = B::Response;
    type Error = A::Error;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        util::ready(&self.svc1, &self.svc2, ctx).await
    }

    #[inline]
    async fn shutdown(&self) {
        util::shutdown(&self.svc1, &self.svc2).await
    }

    #[inline]
    async fn call(
        &self,
        req: Req,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<B::Response, A::Error> {
        let res = ctx.call(&self.svc1, req).await?;
        ctx.call(&self.svc2, res).await
    }
}

#[derive(Debug, Clone)]
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

    #[inline]
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(AndThen {
            svc1: self.svc1.create(cfg.clone()).await?,
            svc2: self.svc2.create(cfg).await?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{chain, chain_factory, fn_factory, Service, ServiceCtx};

    #[derive(Clone)]
    struct Srv1(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<&'static str> for Srv1 {
        type Response = &'static str;
        type Error = ();

        async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn call(
            &self,
            req: &'static str,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, ()> {
            Ok(req)
        }

        async fn shutdown(&self) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<&'static str> for Srv2 {
        type Response = (&'static str, &'static str);
        type Error = ();

        async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn call(
            &self,
            req: &'static str,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, ()> {
            Ok((req, "srv2"))
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
            .and_then(Srv2(cnt.clone(), cnt_sht.clone()))
            .clone()
            .into_pipeline();
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
        assert_eq!(cnt.get(), 2);
        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);
    }

    #[ntex::test]
    async fn test_ready2() {
        let cnt = Rc::new(Cell::new(0));
        let srv = Box::new(
            chain(Srv1(cnt.clone(), Rc::new(Cell::new(0))))
                .and_then(Srv2(cnt.clone(), Rc::new(Cell::new(0)))),
        )
        .into_pipeline();
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone(), Rc::new(Cell::new(0))))
            .and_then(Srv2(cnt, Rc::new(Cell::new(0))))
            .into_pipeline();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }

    #[ntex::test]
    async fn test_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let new_srv = chain_factory(fn_factory(move || {
            let cnt = cnt2.clone();
            async move { Ok::<_, ()>(Srv1(cnt, Rc::new(Cell::new(0)))) }
        }))
        .and_then(fn_factory(move || {
            let cnt = cnt.clone();
            async move { Ok(Srv2(cnt.clone(), Rc::new(Cell::new(0)))) }
        }))
        .clone();

        let srv = new_srv.pipeline(&()).await.unwrap();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }
}
