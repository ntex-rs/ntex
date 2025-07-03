#![allow(clippy::type_complexity)]
use std::{fmt, future::Future, marker};

use super::{
    IntoService, IntoServiceFactory, Pipeline, Service, ServiceCtx, ServiceFactory,
};

/// Apply transform function to a service.
pub fn apply_fn<T, Req, F, R, In, Out, Err, U>(
    service: U,
    f: F,
) -> Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
    F: Fn(In, Pipeline<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
    U: IntoService<T, Req>,
    Err: From<T::Error>,
{
    Apply::new(service.into_service(), f)
}

/// Service factory that produces `apply_fn` service.
pub fn apply_fn_factory<T, Req, Cfg, F, R, In, Out, Err, U>(
    service: U,
    f: F,
) -> ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    U: IntoServiceFactory<T, Req, Cfg>,
    Err: From<T::Error>,
{
    ApplyFactory::new(service.into_factory(), f)
}

/// `Apply` service combinator
pub struct Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
{
    service: Pipeline<T>,
    f: F,
    r: marker::PhantomData<fn(Req) -> (In, Out, R, Err)>,
}

impl<T, Req, F, R, In, Out, Err> Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
    F: Fn(In, Pipeline<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    pub(crate) fn new(service: T, f: F) -> Self {
        Apply {
            f,
            service: Pipeline::new(service),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, F, R, In, Out, Err> Clone for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req> + Clone,
    F: Fn(In, Pipeline<T>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    fn clone(&self) -> Self {
        Apply {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, F, R, In, Out, Err> fmt::Debug for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Apply")
            .field("service", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, Req, F, R, In, Out, Err> Service<In> for Apply<T, Req, F, R, In, Out, Err>
where
    T: Service<Req>,
    F: Fn(In, Pipeline<T>) -> R,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    type Response = Out;
    type Error = Err;

    #[inline]
    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Err> {
        self.service.ready().await.map_err(From::from)
    }

    #[inline]
    async fn call(
        &self,
        req: In,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        (self.f)(req, self.service.clone()).await
    }

    crate::forward_poll!(service);
    crate::forward_shutdown!(service);
}

/// `apply()` service factory
pub struct ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
{
    service: T,
    f: F,
    r: marker::PhantomData<fn(Req, Cfg) -> (R, In, Out)>,
}

impl<T, Req, Cfg, F, R, In, Out, Err> ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub(crate) fn new(service: T, f: F) -> Self {
        Self {
            f,
            service,
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, Cfg, F, R, In, Out, Err> Clone
    for ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg> + Clone,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            f: self.f.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<T, Req, Cfg, F, R, In, Out, Err> fmt::Debug
    for ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg> + fmt::Debug,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.service)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<T, Req, Cfg, F, R, In, Out, Err> ServiceFactory<In, Cfg>
    for ApplyFactory<T, Req, Cfg, F, R, In, Out, Err>
where
    T: ServiceFactory<Req, Cfg>,
    F: Fn(In, Pipeline<T::Service>) -> R + Clone,
    R: Future<Output = Result<Out, Err>>,
    Err: From<T::Error>,
{
    type Response = Out;
    type Error = Err;

    type Service = Apply<T::Service, Req, F, R, In, Out, Err>;
    type InitError = T::InitError;

    #[inline]
    async fn create(&self, cfg: Cfg) -> Result<Self::Service, Self::InitError> {
        self.service.create(cfg).await.map(|svc| Apply {
            service: svc.into(),
            f: self.f.clone(),
            r: marker::PhantomData,
        })
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::lazy;
    use std::{cell::Cell, rc::Rc, task::Context};

    use super::*;
    use crate::{chain, chain_factory, fn_factory};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();

        async fn call(&self, _: (), _: ServiceCtx<'_, Self>) -> Result<(), ()> {
            Ok(())
        }

        fn poll(&self, _: &mut Context<'_>) -> Result<(), Self::Error> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn shutdown(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Err;

    impl From<()> for Err {
        fn from(_: ()) -> Self {
            Err
        }
    }

    #[ntex::test]
    async fn test_call() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = chain(
            apply_fn(Srv(cnt_sht.clone()), |req: &'static str, svc| async move {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone(),
        )
        .into_pipeline();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        lazy(|cx| srv.poll(cx)).await.unwrap();
        assert_eq!(cnt_sht.get(), 1);

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }

    #[ntex::test]
    async fn test_call_chain() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = chain(Srv(cnt_sht.clone()))
            .apply_fn(|req: &'static str, svc| async move {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone()
            .into_pipeline();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 1);

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_create() {
        let new_srv = chain_factory(
            apply_fn_factory(
                fn_factory(|| async { Ok::<_, ()>(Srv::default()) }),
                |req: &'static str, srv| async move {
                    srv.call(()).await.unwrap();
                    Ok((req, ()))
                },
            )
            .clone(),
        );

        let srv = new_srv.pipeline(&()).await.unwrap();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");

        assert!(Err == Err::from(()));
    }

    #[ntex::test]
    async fn test_create_chain() {
        let new_srv = chain_factory(fn_factory(|| async { Ok::<_, ()>(Srv::default()) }))
            .apply_fn(|req: &'static str, srv| async move {
                srv.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone();

        let srv = new_srv.pipeline(&()).await.unwrap();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");
    }
}
