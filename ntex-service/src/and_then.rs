use std::marker::PhantomData;

use super::{Service, ServiceCtx, ServiceFactory, util};

#[derive(Clone, Debug)]
/// Service for the `and_then` combinator, chaining a computation onto the end
/// of another service which completes successfully.
///
/// This is created by the `ServiceChain::and_then` method.
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
    B: Service<A::Response, Error = A::Error, Data = A::Data>,
{
    type Response = B::Response;
    type Error = A::Error;
    type Data = A::Data;

    #[inline]
    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        util::ready(&self.svc1, &self.svc2, data, data, ctx).await
    }

    #[inline]
    fn poll(
        &self,
        data: &Self::Data,
        cx: &mut std::task::Context<'_>,
    ) -> Result<(), Self::Error> {
        self.svc1.poll(data, cx)?;
        self.svc2.poll(data, cx)
    }

    #[inline]
    async fn shutdown(&self, data: &Self::Data) {
        util::shutdown(&self.svc1, &self.svc2, data, data).await;
    }

    #[inline]
    async fn call(
        &self,
        req: Req,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<B::Response, A::Error> {
        let result = ctx.call(&self.svc1, req, data).await?;
        ctx.call(&self.svc2, result, data).await
    }
}

#[derive(Debug, Clone)]
/// `.and_then()` service factory combinator
pub struct AndThenFactory<A, B, Req> {
    svc1: A,
    svc2: B,
    _t: PhantomData<fn(Req)>,
}

impl<A, B, Req> AndThenFactory<A, B, Req> {
    /// Create new `AndThenFactory` combinator
    pub fn new(svc1: A, svc2: B) -> Self {
        Self {
            svc1,
            svc2,
            _t: PhantomData,
        }
    }
}

impl<A, B, Req, Cfg> ServiceFactory<Req, Cfg> for AndThenFactory<A, B, Req>
where
    A: ServiceFactory<Req, Cfg>,
    B: ServiceFactory<
            A::Response,
            Cfg,
            Error = A::Error,
            InitError = A::InitError,
            Data = A::Data,
        >,
    B::Service:
        Service<A::Response, Error = A::Error, Data = <A::Service as Service<Req>>::Data>,
    Cfg: Clone,
{
    type Response = B::Response;
    type Error = B::Error;
    type Service = AndThen<A::Service, B::Service>;
    type InitError = A::InitError;
    type Data = A::Data;

    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError> {
        let svc1 = self.svc1.create(cfg.clone()).await?;
        let svc2 = self.svc2.create(cfg).await?;
        Ok(AndThen { svc1, svc2 })
    }

    async fn map_data(
        &self,
        cfg: &Cfg,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<Req>>::Data, Self::InitError> {
        let svc_data = self.svc1.map_data(cfg, data).await?;
        self.svc2.map_data(cfg, data).await?;
        Ok(svc_data)
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use ntex::util::lazy;
    use std::{cell::Cell, rc::Rc, task::Context};

    use crate::{Service, ServiceCtx, chain, chain_factory, fn_factory};

    #[derive(Debug, Clone)]
    struct Srv1(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<&'static str> for Srv1 {
        type Response = &'static str;
        type Error = ();
        type Data = ();

        async fn ready(
            &self,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        fn poll(&self, _: &Self::Data, _: &mut Context<'_>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn call(
            &self,
            req: &'static str,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, ()> {
            Ok(req)
        }

        async fn shutdown(&self, _: &Self::Data) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[derive(Debug, Clone)]
    struct Srv2(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<&'static str> for Srv2 {
        type Response = (&'static str, &'static str);
        type Error = ();
        type Data = ();

        async fn ready(
            &self,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        fn poll(&self, _: &Self::Data, _: &mut Context<'_>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn call(
            &self,
            req: &'static str,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, ()> {
            Ok((req, "srv2"))
        }

        async fn shutdown(&self, _: &Self::Data) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = chain(Box::new(Srv1(cnt.clone(), cnt_sht.clone())))
            .clone()
            .and_then(crate::boxed::service(Srv2(cnt.clone(), cnt_sht.clone())))
            .into_pipeline(());
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
        assert_eq!(cnt.get(), 2);

        lazy(|cx| srv.clone().poll(cx)).await.unwrap();
        assert_eq!(cnt.get(), 4);

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        assert!(format!("{srv:?}").contains("AndThen"));
    }

    #[ntex::test]
    async fn test_ready2() {
        let cnt = Rc::new(Cell::new(0));
        let srv = Box::new(
            chain(Srv1(cnt.clone(), Rc::new(Cell::new(0))))
                .and_then(Srv2(cnt.clone(), Rc::new(Cell::new(0)))),
        )
        .into_pipeline(());
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_call() {
        let cnt = Rc::new(Cell::new(0));
        let srv = chain(Box::new(Srv1(cnt.clone(), Rc::new(Cell::new(0)))))
            .and_then(Srv2(cnt, Rc::new(Cell::new(0))))
            .into_pipeline(());
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

        let srv = new_srv.pipeline(&(), &()).await.unwrap();
        let res = srv.call("srv1").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "srv2"));
    }
}
