use std::fmt;

use super::{Ctx, ReadyCtx, Service, ServiceFactory};

/// Service for the `inspect` combinator.
pub struct Inspect<S, F> {
    svc: S,
    f: F,
}

impl<S, F> Inspect<S, F> {
    /// Create new `Inspect` service combinator.
    pub(crate) fn new<St>(svc: S, f: F) -> Self
    where
        S: Service<St>,
        F: Fn(&S::Res),
    {
        Self { svc, f }
    }
}

impl<S, F> Clone for Inspect<S, F>
where
    S: Clone,
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Inspect {
            svc: self.svc.clone(),
            f: self.f.clone(),
        }
    }
}

impl<S, F> fmt::Debug for Inspect<S, F>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inspect")
            .field("svc", &self.svc)
            .field("inspect", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, F, St> Service<St> for Inspect<S, F>
where
    S: Service<St>,
    F: Fn(&S::Res),
{
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: S::Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        ctx.call(&self.svc, req).await.inspect(&self.f)
    }

    crate::forward_ready!(St, svc);
    crate::forward_shutdown!(svc);
}

/// Service for the `inspect_err` combinator.
pub struct InspectErr<S, F> {
    svc: S,
    f: F,
}

impl<S, F> InspectErr<S, F> {
    /// Create new `InspectErr` service combinator.
    pub(crate) fn new<St>(svc: S, f: F) -> Self
    where
        S: Service<St>,
        F: Fn(&S::Error),
    {
        Self { svc, f }
    }
}

impl<S, F> Clone for InspectErr<S, F>
where
    S: Clone,
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        InspectErr {
            svc: self.svc.clone(),
            f: self.f.clone(),
        }
    }
}

impl<S, F> fmt::Debug for InspectErr<S, F>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InspectErr")
            .field("svc", &self.svc)
            .field("inspect_err", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, F, St> Service<St> for InspectErr<S, F>
where
    S: Service<St>,
    F: Fn(&S::Error),
{
    type Req = S::Req;
    type Res = S::Res;
    type Error = S::Error;

    #[inline]
    async fn call(&self, req: S::Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        ctx.call(&self.svc, req).await.inspect_err(&self.f)
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(&self.svc).await.inspect_err(&self.f)
    }

    crate::forward_shutdown!(svc);
}

/// Factory for the `inspect` combinator.
pub struct InspectFactory<S, F> {
    s: S,
    f: F,
}

impl<S, F> InspectFactory<S, F> {
    /// Create new `InspectFactory` factory instance.
    pub(crate) fn new(s: S, f: F) -> Self {
        Self { s, f }
    }
}

impl<S, F> Clone for InspectFactory<S, F>
where
    S: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            s: self.s.clone(),
            f: self.f.clone(),
        }
    }
}

impl<S, F> fmt::Debug for InspectFactory<S, F>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InspectFactory")
            .field("factory", &self.s)
            .field("inspect", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Sf, St, Req, F> ServiceFactory<Req, St> for InspectFactory<Sf, F>
where
    Sf: ServiceFactory<Req, St>,
    F: Fn(&Sf::Res) + Clone,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = Inspect<Sf::Service, F>;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.s.create(cfg).await.map(|svc| Inspect {
            svc,
            f: self.f.clone(),
        })
    }
}

/// Factory for the `inspect_err` combinator.
pub struct InspectErrFactory<Sf, F> {
    s: Sf,
    f: F,
}

impl<Sf, F> InspectErrFactory<Sf, F> {
    /// Create new `InspectErrFactory` factory instance.
    pub(crate) fn new(s: Sf, f: F) -> Self {
        Self { s, f }
    }
}

impl<Sf, F> Clone for InspectErrFactory<Sf, F>
where
    Sf: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            s: self.s.clone(),
            f: self.f.clone(),
        }
    }
}

impl<Sf, F> fmt::Debug for InspectErrFactory<Sf, F>
where
    Sf: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InspectErrFactory")
            .field("factory", &self.s)
            .field("inspect_err", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Sf, St, Req, F> ServiceFactory<Req, St> for InspectErrFactory<Sf, F>
where
    Sf: ServiceFactory<Req, St>,
    F: Fn(&Sf::Error) + Clone,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = InspectErr<Sf::Service, F>;
    type InitCfg = Sf::InitCfg;
    type InitError = Sf::InitError;

    #[inline]
    async fn create(&self, cfg: &Sf::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.s.create(cfg).await.map(|svc| InspectErr {
            svc,
            f: self.f.clone(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{chain, factory, fn_factory};

    #[derive(Debug, Clone)]
    struct Srv(bool, bool, Rc<Cell<usize>>);

    impl Service for Srv {
        type Req = ();
        type Res = ();
        type Error = ();

        async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
            if self.1 { Err(()) } else { Ok(()) }
        }

        async fn call(&self, _m: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            if self.0 { Err(()) } else { Ok(()) }
        }

        async fn shutdown(&self) {
            self.2.set(self.2.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_inspect_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let srv = chain(Srv(false, false, cnt.clone()))
            .inspect(move |&()| cnt2.set(cnt2.get() + 1))
            .into_pipeline();
        let res = srv.ready().await;
        assert_eq!(res, Ok(()));

        srv.shutdown().await;
        assert_eq!(cnt.get(), 1);
    }

    #[ntex::test]
    async fn test_inspect_err_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let srv = chain(Srv(true, true, cnt.clone()))
            .inspect_err(move |&()| cnt2.set(cnt2.get() + 1))
            .into_pipeline();
        let res = srv.ready().await;
        assert_eq!(res, Err(()));

        srv.shutdown().await;
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_inspect_service() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let srv = chain(Srv(false, false, cnt.clone()))
            .inspect(move |&()| cnt2.set(cnt2.get() + 1))
            .clone()
            .into_pipeline();
        let res = srv.call(()).await;
        assert!(res.is_ok());

        let _ = format!("{srv:?}");

        srv.shutdown().await;
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_inspect_err_service() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let srv = chain(Srv(false, true, cnt.clone()))
            .inspect_err(move |&()| cnt2.set(cnt2.get() + 1))
            .clone()
            .into_pipeline();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), ());

        let _ = format!("{srv:?}");

        srv.shutdown().await;
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_inspect_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let cnt3 = cnt.clone();
        let new_srv = factory(fn_factory(async move || {
            Ok::<_, ()>(Srv(false, false, cnt2.clone()))
        }))
        .inspect(move |&()| cnt3.set(cnt3.get() + 1))
        .clone();
        let srv = new_srv.pipeline(&()).await.unwrap();
        let res = srv.call(()).await;
        assert!(res.is_ok());
        let _ = format!("{new_srv:?}");
        srv.shutdown().await;
        assert_eq!(cnt.get(), 2);
    }

    #[ntex::test]
    async fn test_inspect_err_factory() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let cnt3 = cnt.clone();
        let new_srv = factory(fn_factory(async move || {
            Ok::<_, ()>(Srv(false, true, cnt2.clone()))
        }))
        .inspect_err(move |&()| cnt3.set(cnt3.get() + 1))
        .clone();
        let srv = new_srv.pipeline(&()).await.unwrap();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), ());
        let _ = format!("{new_srv:?}");
        srv.shutdown().await;
        assert_eq!(cnt.get(), 2);
    }
}
