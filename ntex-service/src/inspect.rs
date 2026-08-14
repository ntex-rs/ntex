use std::{fmt, task::Context};

use super::{Service, ServiceCtx, ServiceFactory};
use crate::svc_fct::{ErrorOf, ResponseOf, ServiceOf};

/// Service for the `inspect` combinator.
pub struct Inspect<S, F> {
    svc: S,
    f: F,
}

impl<S, F> Inspect<S, F> {
    /// Create new `Inspect` service combinator.
    pub(crate) fn new<R>(svc: S, f: F) -> Self
    where
        S: Service<R>,
        F: Fn(&S::Response),
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

impl<S, F, R> Service<R> for Inspect<S, F>
where
    S: Service<R>,
    F: Fn(&S::Response),
{
    type Response = S::Response;
    type Error = S::Error;
    type Data = S::Data;

    #[inline]
    async fn call(
        &self,
        r: R,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<S::Response, S::Error> {
        ctx.call(&self.svc, r, data).await.inspect(&self.f)
    }

    crate::forward_ready!(svc);
    crate::forward_poll!(svc);
    crate::forward_shutdown!(svc);
}

/// Service for the `inspect_err` combinator.
pub struct InspectErr<S, F> {
    svc: S,
    f: F,
}

impl<S, F> InspectErr<S, F> {
    /// Create new `InspectErr` service combinator.
    pub(crate) fn new<R>(svc: S, f: F) -> Self
    where
        S: Service<R>,
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

impl<S, F, R> Service<R> for InspectErr<S, F>
where
    S: Service<R>,
    F: Fn(&S::Error),
{
    type Response = S::Response;
    type Error = S::Error;
    type Data = S::Data;

    #[inline]
    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        ctx.ready(&self.svc, data).await.inspect_err(&self.f)
    }

    #[inline]
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        self.svc.poll(data, cx).inspect_err(&self.f)
    }

    #[inline]
    async fn call(
        &self,
        r: R,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<S::Response, S::Error> {
        ctx.call(&self.svc, r, data).await.inspect_err(&self.f)
    }

    crate::forward_shutdown!(svc);
}

/// Factory for the `inspect` combinator.
pub struct InspectFactory<S, F, R> {
    s: S,
    f: F,
    _t: std::marker::PhantomData<fn(R)>,
}

impl<S, F, R> InspectFactory<S, F, R> {
    /// Create new `InspectFactory` factory instance.
    pub(crate) fn new(s: S, f: F) -> Self {
        Self {
            s,
            f,
            _t: std::marker::PhantomData,
        }
    }
}

impl<S, F, R> Clone for InspectFactory<S, F, R>
where
    S: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            s: self.s.clone(),
            f: self.f.clone(),
            _t: std::marker::PhantomData,
        }
    }
}

impl<S, F, R> fmt::Debug for InspectFactory<S, F, R>
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

impl<S, F, R, C> ServiceFactory<R, C> for InspectFactory<S, F, R>
where
    S: ServiceFactory<R, C>,
    F: Fn(&ResponseOf<S, R, C>) + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = Inspect<ServiceOf<S, R, C>, F>;
    type InitError = S::InitError;
    type Data = S::Data;

    #[inline]
    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.s.create(cfg).await.map(|svc| Inspect {
            svc,
            f: self.f.clone(),
        })
    }

    #[inline]
    async fn map_data(
        &self,
        cfg: &C,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<R>>::Data, Self::InitError> {
        self.s.map_data(cfg, data).await
    }
}

/// Factory for the `inspect_err` combinator.
pub struct InspectErrFactory<S, F, R> {
    s: S,
    f: F,
    _t: std::marker::PhantomData<fn(R)>,
}

impl<S, F, R> InspectErrFactory<S, F, R> {
    /// Create new `InspectErrFactory` factory instance.
    pub(crate) fn new(s: S, f: F) -> Self {
        Self {
            s,
            f,
            _t: std::marker::PhantomData,
        }
    }
}

impl<S, F, R> Clone for InspectErrFactory<S, F, R>
where
    S: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            s: self.s.clone(),
            f: self.f.clone(),
            _t: std::marker::PhantomData,
        }
    }
}

impl<S, F, R> fmt::Debug for InspectErrFactory<S, F, R>
where
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InspectErrFactory")
            .field("factory", &self.s)
            .field("inspect_err", &std::any::type_name::<F>())
            .finish()
    }
}

impl<S, F, R, C> ServiceFactory<R, C> for InspectErrFactory<S, F, R>
where
    S: ServiceFactory<R, C>,
    F: Fn(&ErrorOf<S, R, C>) + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Service = InspectErr<ServiceOf<S, R, C>, F>;
    type InitError = S::InitError;
    type Data = S::Data;

    #[inline]
    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        self.s.create(cfg).await.map(|svc| InspectErr {
            svc,
            f: self.f.clone(),
        })
    }

    #[inline]
    async fn map_data(
        &self,
        cfg: &C,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<R>>::Data, Self::InitError> {
        self.s.map_data(cfg, data).await
    }
}

#[cfg(test)]
#[allow(clippy::unused_async_trait_impl)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use super::*;
    use crate::{chain, chain_factory, fn_factory};

    #[derive(Debug, Clone)]
    struct Srv(bool, bool, Rc<Cell<usize>>);

    impl Service<()> for Srv {
        type Response = ();
        type Error = ();
        type Data = ();

        async fn ready(
            &self,
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<(), Self::Error> {
            if self.1 { Err(()) } else { Ok(()) }
        }

        async fn call(
            &self,
            _m: (),
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<(), ()> {
            if self.0 { Err(()) } else { Ok(()) }
        }

        async fn shutdown(&self, _: &Self::Data) {
            self.2.set(self.2.get() + 1);
        }
    }

    #[ntex::test]
    async fn test_inspect_ready() {
        let cnt = Rc::new(Cell::new(0));
        let cnt2 = cnt.clone();
        let srv = chain(Srv(false, false, cnt.clone()))
            .inspect(move |&()| cnt2.set(cnt2.get() + 1))
            .into_pipeline(());
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
            .into_pipeline(());
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
            .into_pipeline(());
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
            .into_pipeline(());
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
        let new_srv = chain_factory(fn_factory(async move || {
            Ok::<_, ()>(Srv(false, false, cnt2.clone()))
        }))
        .inspect(move |&()| cnt3.set(cnt3.get() + 1))
        .clone();
        let srv = new_srv.pipeline(&(), &()).await.unwrap();
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
        let new_srv = chain_factory(fn_factory(async move || {
            Ok::<_, ()>(Srv(false, true, cnt2.clone()))
        }))
        .inspect_err(move |&()| cnt3.set(cnt3.get() + 1))
        .clone();
        let srv = new_srv.pipeline(&(), &()).await.unwrap();
        let res = srv.call(()).await;
        assert!(res.is_err());
        assert_eq!(res.err().unwrap(), ());
        let _ = format!("{new_srv:?}");
        srv.shutdown().await;
        assert_eq!(cnt.get(), 2);
    }
}
