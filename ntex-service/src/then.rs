use std::marker::PhantomData;

use super::{Service, ServiceCtx, ServiceFactory, util};
use crate::svc_fct::{ErrorOf, ResponseOf, ServiceOf};

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
    B: Service<Result<A::Response, A::Error>, Error = A::Error, Data = A::Data>,
{
    type Response = B::Response;
    type Error = B::Error;
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
        req: R,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        ctx.call(&self.svc2, ctx.call(&self.svc1, req, data).await, data)
            .await
    }
}

#[derive(Debug, Clone)]
/// `.then()` service factory combinator
pub struct ThenFactory<A, B, R> {
    svc1: A,
    svc2: B,
    _t: PhantomData<fn(R)>,
}

impl<A, B, R> ThenFactory<A, B, R> {
    /// Create new factory for `Then` combinator
    pub(crate) fn new(svc1: A, svc2: B) -> Self {
        Self {
            svc1,
            svc2,
            _t: PhantomData,
        }
    }
}

impl<A, B, R, C> ServiceFactory<R, C> for ThenFactory<A, B, R>
where
    A: ServiceFactory<R, C>,
    B: ServiceFactory<
            Result<ResponseOf<A, R, C>, ErrorOf<A, R, C>>,
            C,
            Error = ErrorOf<A, R, C>,
            InitError = A::InitError,
            Data = A::Data,
        >,
    B::Service: Service<
            Result<ResponseOf<A, R, C>, ErrorOf<A, R, C>>,
            Error = ErrorOf<A, R, C>,
            Data = <A::Service as Service<R>>::Data,
        >,
    C: Clone,
{
    type Response = B::Response;
    type Error = B::Error;
    type Service = Then<
        ServiceOf<A, R, C>,
        ServiceOf<B, Result<ResponseOf<A, R, C>, ErrorOf<A, R, C>>, C>,
    >;
    type InitError = A::InitError;
    type Data = A::Data;

    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        Ok(Then {
            svc1: self.svc1.create(cfg.clone()).await?,
            svc2: self.svc2.create(cfg).await?,
        })
    }

    async fn map_data(
        &self,
        cfg: &C,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<R>>::Data, Self::InitError> {
        let svc_data = self.svc1.map_data(cfg, data).await?;
        self.svc2.map_data(cfg, data).await?;
        Ok(svc_data)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use ntex::util::lazy;
    use std::{cell::Cell, rc::Rc, task::Context};

    use crate::{Service, ServiceCtx, chain, chain_factory, fn_factory};

    #[derive(Clone)]
    struct Srv1(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<Result<&'static str, &'static str>> for Srv1 {
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
            req: Result<&'static str, &'static str>,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<&'static str, ()> {
            match req {
                Ok(msg) => Ok(msg),
                Err(_) => Err(()),
            }
        }

        async fn shutdown(&self, _: &Self::Data) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[derive(Clone)]
    struct Srv2(Rc<Cell<usize>>, Rc<Cell<usize>>);

    impl Service<Result<&'static str, ()>> for Srv2 {
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
            req: Result<&'static str, ()>,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<Self::Response, ()> {
            match req {
                Ok(msg) => Ok((msg, "ok")),
                Err(()) => Ok(("srv2", "err")),
            }
        }

        async fn shutdown(&self, _: &Self::Data) {
            self.1.set(self.1.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = chain(Srv1(cnt.clone(), cnt_sht.clone()))
            .then(Srv2(cnt.clone(), cnt_sht.clone()))
            .into_pipeline(());
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
            .into_pipeline(());

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
        let srv = factory.pipeline(&(), &()).await.unwrap();
        let res = srv.call(Ok("srv1")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv1", "ok"));

        let res = srv.call(Err("srv")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv2", "err"));
    }
}
