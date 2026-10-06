use std::{cell::Cell, fmt, marker};

use crate::ctx::{Ctx, WaitersRef};
use crate::{IntoService, IntoServiceFactory, Service, ServiceFactory};
use crate::{ServiceCaller, ServiceChain, ServiceChainFactory};

/// Applies an asynchronous middleware function to a service.
///
/// The function receives an input request and an [`ApplyCtx`] that can call the
/// wrapped service.
pub fn apply_fn<S, St, Req, F, In, Out, Err>(
    service: impl IntoService<S, St, Req>,
    f: F,
) -> ServiceChain<Apply<S, St, Req, F, In, Out, Err>, St, In>
where
    S: Service<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
    crate::service(Apply::new(service.into_service(), f))
}

/// Applies an asynchronous middleware function to every service from a factory.
pub fn apply_fn_factory<Sf, St, Req, F, In, Out, Err>(
    service: impl IntoServiceFactory<Sf, St, Req>,
    f: F,
) -> ServiceChainFactory<ApplyFactory<F, Sf, St, Req, In, Out, Err>, St, In>
where
    Sf: ServiceFactory<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Err: From<Sf::Error>,
{
    crate::factory(ApplyFactory::new(service.into_factory(), f))
}

#[derive(Debug)]
/// Context passed to middleware functions created by [`apply_fn`].
pub struct ApplyCtx<'a, S, St, Req> {
    idx: u32,
    waiters: &'a WaitersRef,
    service: &'a S,
    st: &'a St,
    ready: &'a Cell<bool>,
    entered: &'a Cell<u32>,
    r: marker::PhantomData<Req>,
}

impl<S: Service<St, Req>, St, Req> ApplyCtx<'_, S, St, Req> {
    /// Returns the pipeline state.
    #[inline]
    pub fn st(&self) -> &St {
        self.st
    }

    /// Waits for the wrapped service to become ready, then calls it.
    #[inline]
    pub async fn call(&self, req: Req) -> Result<S::Res, S::Error> {
        self.enter(req, self.ready.get()).await
    }

    async fn enter(&self, req: Req, skip_ready: bool) -> Result<S::Res, S::Error> {
        let ctx = Ctx::<S, St>::new(self.idx, self.waiters, self.st);
        if !skip_ready {
            ctx.ready(self.service).await?;
        }
        // entering the service invalidates all readiness observed so far
        self.ready.set(false);
        self.entered.set(self.entered.get().wrapping_add(1));
        ctx.call_nowait(self.service, req).await
    }
}

impl<S: Service<St, Req>, St, Req> ServiceCaller<Req, S::Res, S::Error>
    for ApplyCtx<'_, S, St, Req>
{
    #[inline]
    async fn call_service(&self, req: Req) -> Result<S::Res, S::Error> {
        self.enter(req, false).await
    }
}

/// Service produced by [`apply_fn`].
pub struct Apply<S, St, Req, F, In, Out, Err> {
    svc: S,
    f: F,
    ready: Cell<bool>,
    entered: Cell<u32>,
    r: marker::PhantomData<fn(St, Req) -> (In, Out, Err)>,
}

impl<S, St, Req, F, In, Out, Err> Apply<S, St, Req, F, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
{
    pub(crate) fn new(svc: S, f: F) -> Self {
        Apply {
            f,
            svc,
            ready: Cell::new(false),
            entered: Cell::new(0),
            r: marker::PhantomData,
        }
    }
}

impl<S, St, Req, F, In, Out, Err> Clone for Apply<S, St, Req, F, In, Out, Err>
where
    S: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Apply {
            svc: self.svc.clone(),
            f: self.f.clone(),
            ready: Cell::new(false),
            entered: Cell::new(0),
            r: marker::PhantomData,
        }
    }
}

impl<S, St, Req, F, In, Out, Err> fmt::Debug for Apply<S, St, Req, F, In, Out, Err>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Apply")
            .field("svc", &self.svc)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, St, Req, F, In, Out, Err> Service<St, In> for Apply<S, St, Req, F, In, Out, Err>
where
    S: Service<St, Req>,
    F: AsyncFn(In, &ApplyCtx<'_, S, St, Req>) -> Result<Out, Err>,
    Err: From<S::Error>,
{
    type Res = Out;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Err> {
        let entered = self.entered.get();
        let result = ctx.ready(&self.svc).await.map_err(From::from);
        if result.is_err() {
            self.ready.set(false);
        } else if self.entered.get() == entered {
            // readiness is valid only if no call entered the service during the check
            self.ready.set(true);
        }
        result
    }

    #[inline]
    async fn call(&self, req: In, ctx: Ctx<'_, Self, St>) -> Result<Out, Err> {
        let (idx, waiters, st) = ctx.inner();

        let ctx = ApplyCtx {
            idx,
            waiters,
            st,
            ready: &self.ready,
            entered: &self.entered,
            service: &self.svc,
            r: marker::PhantomData,
        };
        (self.f)(req, &ctx).await
    }

    crate::forward_shutdown!(St, svc);
}

/// Service factory produced by [`apply_fn_factory`].
pub struct ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req>,
{
    f: F,
    sf: Sf,
    r: marker::PhantomData<fn(St, Req) -> (In, Out)>,
}

impl<F, Sf, St, Req, In, Out, Err> ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req>,
{
    /// Creates a new `ApplyFactory`.
    pub(crate) fn new(sf: Sf, f: F) -> Self
    where
        Sf: ServiceFactory<St, Req>,
        Err: From<Sf::Error>,
    {
        Self {
            f,
            sf,
            r: marker::PhantomData,
        }
    }
}

impl<F, Sf, St, Req, In, Out, Err> Clone for ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            sf: self.sf.clone(),
            r: marker::PhantomData,
        }
    }
}

impl<F, Sf, St, Req, In, Out, Err> fmt::Debug for ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyFactory")
            .field("factory", &self.sf)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Sf, St, Req, In, Out, Err> ServiceFactory<St, In>
    for ApplyFactory<F, Sf, St, Req, In, Out, Err>
where
    F: AsyncFn(In, &ApplyCtx<'_, Sf::Service, St, Req>) -> Result<Out, Err> + Clone,
    Sf: ServiceFactory<St, Req>,
    Err: From<Sf::Error>,
{
    type Res = Out;
    type Error = Err;

    type Service = Apply<Sf::Service, St, Req, F, In, Out, Err>;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        self.sf.create(st).await.map(|svc| Apply {
            svc,
            f: self.f.clone(),
            r: marker::PhantomData,
            ready: Cell::new(false),
            entered: Cell::new(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{factory, fn_factory, service};

    #[derive(Debug, Default, Clone)]
    struct Srv(Rc<Cell<usize>>);

    impl Service<(), ()> for Srv {
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            Ok(())
        }

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
            self.0.set(self.0.get() + 1);
            Ok(())
        }

        async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Err;

    impl From<()> for Err {
        fn from(_e: ()) -> Self {
            Err
        }
    }

    #[ntex::test]
    async fn test_call() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = service(
            apply_fn(Srv(cnt_sht.clone()), async move |req: &'static str, svc| {
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone(),
        )
        .pipeline(());

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
    }

    #[ntex::test]
    async fn test_call_svc() {
        let cnt_sht = Rc::new(Cell::new(0));
        let srv = service(Srv(cnt_sht.clone()))
            .apply_fn(async move |req: &'static str, svc| {
                svc.st();
                svc.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone();
        let s = format!("{srv:?}");
        assert!(s.contains("Apply"), "{}", s);

        let srv = srv.pipeline(());
        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        srv.shutdown().await;
        assert_eq!(cnt_sht.get(), 2);

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{srv:?}");
    }

    #[ntex::test]
    async fn test_create() {
        let new_srv = factory(apply_fn_factory(
            fn_factory(|(): &()| async { Ok::<_, ()>(Srv::default()) }),
            async move |req: &'static str, srv| {
                srv.call(()).await.unwrap();
                Ok((req, ()))
            },
        ));

        let srv = new_srv.pipeline(()).await.unwrap();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        assert_eq!(Err, Err::from(()));
    }

    #[ntex::test]
    async fn test_create_chain() {
        let new_srv = factory(fn_factory(|(): &()| async { Ok::<_, ()>(Srv::default()) }))
            .apply_fn(async move |req: &'static str, srv| {
                srv.call(()).await.unwrap();
                Ok((req, ()))
            })
            .clone();

        let srv = new_srv.pipeline(()).await.unwrap();

        assert_eq!(srv.ready().await, Ok::<_, Err>(()));

        let res = srv.call("srv").await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), ("srv", ()));
        let _ = format!("{new_srv:?}");
    }

    mod ready_flag {
        use std::rc::Rc;

        use crate::util::tests::{Req, Single, State, concurrent_entry};
        use crate::{Pipeline, apply::Apply};

        #[ntex::test]
        async fn concurrent_entry_after_await() {
            let st = Rc::new(State::default());
            let pl = Pipeline::new(
                (),
                Apply::new(Single(st.clone()), async |(rx1, rx2): Req, svc| {
                    let _ = rx1.await;
                    svc.call(rx2).await
                }),
            );
            concurrent_entry(&pl, &st).await;
        }
    }
}
