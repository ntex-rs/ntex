//! Either service allows to use different services for handling request
use std::{fmt, task::Context};

use ntex_service::{Ctx, ReadyCtx, Service, ServiceFactory};

use crate::future::Either;

#[derive(Clone)]
/// Either service
///
/// Either service allows to use different services for handling requests
pub struct EitherService<SLeft, SRight> {
    svc: Either<SLeft, SRight>,
}

#[derive(Clone)]
/// Either service factory
///
/// Either service allows to use different services for handling requests
pub struct EitherServiceFactory<ChooseFn, SFLeft, SFRight> {
    left: SFLeft,
    right: SFRight,
    choose_left_fn: ChooseFn,
}

impl<ChooseFn, SFLeft, SFRight> EitherServiceFactory<ChooseFn, SFLeft, SFRight> {
    /// Create `Either` service factory
    pub fn new(choose_left_fn: ChooseFn, sf_left: SFLeft, sf_right: SFRight) -> Self {
        EitherServiceFactory {
            choose_left_fn,
            left: sf_left,
            right: sf_right,
        }
    }
}

impl<ChooseFn, SFLeft, SFRight> fmt::Debug
    for EitherServiceFactory<ChooseFn, SFLeft, SFRight>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EitherServiceFactory")
            .field("left", &std::any::type_name::<SFLeft>())
            .field("right", &std::any::type_name::<SFRight>())
            .field("choose_fn", &std::any::type_name::<ChooseFn>())
            .finish()
    }
}

impl<St, Req, ChooseFn, SFLeft, SFRight> ServiceFactory<St, Req>
    for EitherServiceFactory<ChooseFn, SFLeft, SFRight>
where
    ChooseFn: Fn(&SFLeft::InitCfg) -> bool,
    SFLeft: ServiceFactory<St, Req>,
    SFRight: ServiceFactory<
            St,
            Req,
            Res = SFLeft::Res,
            Error = SFLeft::Error,
            InitCfg = SFLeft::InitCfg,
            InitError = SFLeft::InitError,
        >,
{
    type Res = SFLeft::Res;
    type Error = SFLeft::Error;
    type InitCfg = SFLeft::InitCfg;
    type InitError = SFLeft::InitError;
    type Service = EitherService<SFLeft::Service, SFRight::Service>;

    async fn create(
        &self,
        cfg: &SFLeft::InitCfg,
    ) -> Result<Self::Service, Self::InitError> {
        let choose_left = (self.choose_left_fn)(cfg);

        if choose_left {
            let svc = self.left.create(cfg).await?;
            Ok(EitherService {
                svc: Either::Left(svc),
            })
        } else {
            let svc = self.right.create(cfg).await?;
            Ok(EitherService {
                svc: Either::Right(svc),
            })
        }
    }
}

impl<SLeft, SRight> EitherService<SLeft, SRight> {
    /// Create `Either` service
    pub fn left(svc: SLeft) -> Self {
        EitherService {
            svc: Either::Left(svc),
        }
    }

    /// Create `Either` service
    pub fn right(svc: SRight) -> Self {
        EitherService {
            svc: Either::Right(svc),
        }
    }
}

impl<SLeft, SRight> fmt::Debug for EitherService<SLeft, SRight> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EitherService")
            .field("left", &std::any::type_name::<SLeft>())
            .field("right", &std::any::type_name::<SRight>())
            .finish()
    }
}

impl<SLeft, SRight, St> Service<St> for EitherService<SLeft, SRight>
where
    SLeft: Service<St>,
    SRight: Service<St, Req = SLeft::Req, Res = SLeft::Res, Error = SLeft::Error>,
{
    type Req = SLeft::Req;
    type Res = SLeft::Res;
    type Error = SLeft::Error;

    #[inline]
    async fn call(
        &self,
        req: Self::Req,
        ctx: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        match self.svc {
            Either::Left(ref svc) => ctx.call(svc, req).await,
            Either::Right(ref svc) => ctx.call(svc, req).await,
        }
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        match self.svc {
            Either::Left(ref svc) => ctx.ready(svc).await,
            Either::Right(ref svc) => ctx.ready(svc).await,
        }
    }

    #[inline]
    async fn shutdown(&self) {
        match self.svc {
            Either::Left(ref svc) => svc.shutdown().await,
            Either::Right(ref svc) => svc.shutdown().await,
        }
    }

    #[inline]
    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        match self.svc {
            Either::Left(ref svc) => svc.poll(cx),
            Either::Right(ref svc) => svc.poll(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use ntex_service::{Pipeline, ServiceFactory};

    use super::*;

    #[derive(Copy, Clone, Debug, PartialEq)]
    struct Svc1;
    impl Service<()> for Svc1 {
        type Response = &'static str;
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<&'static str, ()> {
            Ok("svc1")
        }
    }

    #[derive(Clone)]
    struct Svc1Factory;
    impl ServiceFactory<(), &'static str> for Svc1Factory {
        type Response = &'static str;
        type Error = ();
        type InitError = ();
        type Service = Svc1;

        async fn create(&self, _: &'static str) -> Result<Self::Service, Self::InitError> {
            Ok(Svc1)
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq)]
    struct Svc2;
    impl Service<()> for Svc2 {
        type Response = &'static str;
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<&'static str, ()> {
            Ok("svc2")
        }
    }

    #[derive(Clone)]
    struct Svc2Factory;
    impl ServiceFactory<(), &'static str> for Svc2Factory {
        type Response = &'static str;
        type Error = ();
        type InitError = ();
        type Service = Svc2;

        async fn create(&self, _: &'static str) -> Result<Self::Service, Self::InitError> {
            Ok(Svc2)
        }
    }

    type Either = EitherService<Svc1, Svc2>;
    type EitherFactory<F> = EitherServiceFactory<F, Svc1Factory, Svc2Factory>;

    #[ntex::test]
    async fn test_success() {
        let svc = Pipeline::new(Either::left(Svc1).clone());
        assert_eq!(svc.call(()).await, Ok("svc1"));
        assert_eq!(svc.ready().await, Ok(()));
        svc.shutdown().await;

        let svc = Pipeline::new(Either::right(Svc2).clone());
        assert_eq!(svc.call(()).await, Ok("svc2"));
        assert_eq!(svc.ready().await, Ok(()));
        svc.shutdown().await;

        assert!(format!("{svc:?}").contains("EitherService"));
    }

    #[ntex::test]
    async fn test_factory() {
        let factory =
            EitherFactory::new(|s: &&'static str| *s == "svc1", Svc1Factory, Svc2Factory)
                .clone();
        assert!(format!("{factory:?}").contains("EitherServiceFactory"));

        let svc = factory.pipeline("svc1").await.unwrap();
        assert_eq!(svc.call(()).await, Ok("svc1"));

        let svc = factory.pipeline("other").await.unwrap();
        assert_eq!(svc.call(()).await, Ok("svc2"));
    }
}
