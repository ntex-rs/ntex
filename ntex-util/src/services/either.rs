//! Either service allows to use different services for handling request
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

impl<ChooseFn, SFLeft, SFRight> std::fmt::Debug
    for EitherServiceFactory<ChooseFn, SFLeft, SFRight>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EitherServiceFactory")
            .field("left", &std::any::type_name::<SFLeft>())
            .field("right", &std::any::type_name::<SFRight>())
            .field("choose_fn", &std::any::type_name::<ChooseFn>())
            .finish()
    }
}

impl<Req, ChooseFn, SFLeft, SFRight> ServiceFactory<Req>
    for EitherServiceFactory<ChooseFn, SFLeft, SFRight>
where
    ChooseFn: Fn(&SFLeft::InitCfg) -> bool,
    SFLeft: ServiceFactory<Req>,
    SFRight: ServiceFactory<
            Req,
            St = SFLeft::St,
            Res = SFLeft::Res,
            Error = SFLeft::Error,
            InitCfg = SFLeft::InitCfg,
            InitError = SFLeft::InitError,
        >,
{
    type St = SFLeft::St;
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

impl<SLeft, SRight> std::fmt::Debug for EitherService<SLeft, SRight> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EitherService")
            .field("left", &std::any::type_name::<SLeft>())
            .field("right", &std::any::type_name::<SRight>())
            .finish()
    }
}

impl<SL, SR> Service for EitherService<SL, SR>
where
    SL: Service,
    SR: Service<St = SL::St, Req = SL::Req, Res = SL::Res, Error = SL::Error>,
{
    type St = SL::St;
    type Req = SL::Req;
    type Res = SL::Res;
    type Error = SL::Error;

    #[inline]
    async fn call(&self, req: SL::Req, ctx: Ctx<'_, Self>) -> Result<SL::Res, SL::Error> {
        match self.svc {
            Either::Left(ref svc) => ctx.call(svc, req).await,
            Either::Right(ref svc) => ctx.call(svc, req).await,
        }
    }

    #[inline]
    async fn ready(&self, ctx: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use ntex_service::{Pipeline, ServiceFactory};

    use super::*;

    #[derive(Copy, Clone, Debug, PartialEq)]
    struct Svc1;
    impl Service for Svc1 {
        type St = ();
        type Req = ();
        type Res = &'static str;
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<&'static str, ()> {
            Ok("svc1")
        }
    }

    #[derive(Clone)]
    struct Svc1Factory;
    impl ServiceFactory<()> for Svc1Factory {
        type St = ();
        type Res = &'static str;
        type Error = ();

        type Service = Svc1;
        type InitCfg = &'static str;
        type InitError = ();

        async fn create(&self, _: &&'static str) -> Result<Self::Service, Self::InitError> {
            Ok(Svc1)
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq)]
    struct Svc2;
    impl Service for Svc2 {
        type St = ();
        type Req = ();
        type Res = &'static str;
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<&'static str, ()> {
            Ok("svc2")
        }
    }

    #[derive(Clone)]
    struct Svc2Factory;
    impl ServiceFactory<()> for Svc2Factory {
        type St = ();
        type Res = &'static str;
        type Error = ();

        type InitCfg = &'static str;
        type InitError = ();
        type Service = Svc2;

        async fn create(&self, _: &&'static str) -> Result<Self::Service, Self::InitError> {
            Ok(Svc2)
        }
    }

    type Either = EitherService<Svc1, Svc2>;
    type EitherFactory<F> = EitherServiceFactory<F, Svc1Factory, Svc2Factory>;

    #[ntex::test]
    async fn test_success() {
        let svc = Pipeline::new(Either::left(Svc1).clone());
        assert_eq!(svc.call((), &()).await, Ok("svc1"));
        assert_eq!(svc.ready(&()).await, Ok(()));
        svc.shutdown().await;

        let svc = Pipeline::new(Either::right(Svc2).clone());
        assert_eq!(svc.call((), &()).await, Ok("svc2"));
        assert_eq!(svc.ready(&()).await, Ok(()));
        svc.shutdown().await;

        assert!(format!("{svc:?}").contains("EitherService"));
    }

    #[ntex::test]
    async fn test_factory() {
        let factory =
            EitherFactory::new(|s: &&'static str| *s == "svc1", Svc1Factory, Svc2Factory)
                .clone();
        assert!(format!("{factory:?}").contains("EitherServiceFactory"));

        let svc = factory.pipeline(&"svc1").await.unwrap();
        assert_eq!(svc.call((), &()).await, Ok("svc1"));

        let svc = factory.pipeline(&"other").await.unwrap();
        assert_eq!(svc.call((), &()).await, Ok("svc2"));
    }
}
