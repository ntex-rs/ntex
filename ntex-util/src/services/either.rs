//! Either service allows to use different services for handling request
use std::marker::PhantomData;
use std::{fmt, task::Context};

use ntex_service::{Service, ServiceCtx, ServiceFactory};

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
pub struct EitherServiceFactory<ChooseFn, SFLeft, SFRight, Req = ()> {
    left: SFLeft,
    right: SFRight,
    choose_left_fn: ChooseFn,
    _t: PhantomData<fn(Req)>,
}

impl<ChooseFn, SFLeft, SFRight, Req> EitherServiceFactory<ChooseFn, SFLeft, SFRight, Req> {
    /// Create `Either` service factory
    pub fn new(choose_left_fn: ChooseFn, sf_left: SFLeft, sf_right: SFRight) -> Self {
        EitherServiceFactory {
            choose_left_fn,
            left: sf_left,
            right: sf_right,
            _t: PhantomData,
        }
    }
}

impl<ChooseFn, SFLeft, SFRight, Req> fmt::Debug
    for EitherServiceFactory<ChooseFn, SFLeft, SFRight, Req>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EitherServiceFactory")
            .field("left", &std::any::type_name::<SFLeft>())
            .field("right", &std::any::type_name::<SFRight>())
            .field("choose_fn", &std::any::type_name::<ChooseFn>())
            .finish()
    }
}

impl<R, C, ChooseFn, SFLeft, SFRight> ServiceFactory<R, C>
    for EitherServiceFactory<ChooseFn, SFLeft, SFRight, R>
where
    ChooseFn: Fn(&C) -> bool,
    SFLeft: ServiceFactory<R, C>,
    SFRight: ServiceFactory<
            R,
            C,
            Data = SFLeft::Data,
            Error = SFLeft::Error,
            InitError = SFLeft::InitError,
        >,
    SFRight::Service: Service<
            R,
            Response = SFLeft::Response,
            Error = SFLeft::Error,
            Data = <SFLeft::Service as Service<R>>::Data,
        >,
{
    type Response = SFLeft::Response;
    type Error = SFLeft::Error;
    type Service = EitherService<SFLeft::Service, SFRight::Service>;
    type InitError = SFLeft::InitError;
    type Data = SFLeft::Data;

    async fn create(&self, cfg: C) -> Result<Self::Service, Self::InitError> {
        let choose_left = (self.choose_left_fn)(&cfg);

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

    async fn map_data(
        &self,
        cfg: &C,
        data: &Self::Data,
    ) -> Result<<Self::Service as Service<R>>::Data, Self::InitError> {
        if (self.choose_left_fn)(cfg) {
            self.left.map_data(cfg, data).await
        } else {
            self.right.map_data(cfg, data).await
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

impl<Req, SLeft, SRight> Service<Req> for EitherService<SLeft, SRight>
where
    SLeft: Service<Req>,
    SRight:
        Service<Req, Response = SLeft::Response, Error = SLeft::Error, Data = SLeft::Data>,
{
    type Response = SLeft::Response;
    type Error = SLeft::Error;
    type Data = SLeft::Data;

    #[inline]
    async fn ready(
        &self,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<(), Self::Error> {
        match self.svc {
            Either::Left(ref svc) => ctx.ready(svc, data).await,
            Either::Right(ref svc) => ctx.ready(svc, data).await,
        }
    }

    #[inline]
    async fn shutdown(&self, data: &Self::Data) {
        match self.svc {
            Either::Left(ref svc) => svc.shutdown(data).await,
            Either::Right(ref svc) => svc.shutdown(data).await,
        }
    }

    #[inline]
    async fn call(
        &self,
        req: Req,
        data: &Self::Data,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        match self.svc {
            Either::Left(ref svc) => ctx.call(svc, req, data).await,
            Either::Right(ref svc) => ctx.call(svc, req, data).await,
        }
    }

    #[inline]
    fn poll(&self, data: &Self::Data, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        match self.svc {
            Either::Left(ref svc) => svc.poll(data, cx),
            Either::Right(ref svc) => svc.poll(data, cx),
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
        type Data = ();

        async fn call(
            &self,
            _r: (),
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<&'static str, ()> {
            Ok("svc1")
        }
    }

    #[derive(Clone)]
    struct Svc1Factory;
    impl ServiceFactory<(), &'static str> for Svc1Factory {
        type Response = &'static str;
        type Error = ();
        type Service = Svc1;
        type InitError = ();
        type Data = ();

        async fn create(&self, _: &'static str) -> Result<Self::Service, Self::InitError> {
            Ok(Svc1)
        }

        async fn map_data(&self, _: &&'static str, _: &Self::Data) -> Result<(), ()> {
            Ok(())
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq)]
    struct Svc2;
    impl Service<()> for Svc2 {
        type Response = &'static str;
        type Error = ();
        type Data = ();

        async fn call(
            &self,
            _r: (),
            _: &Self::Data,
            _: ServiceCtx<'_, Self>,
        ) -> Result<&'static str, ()> {
            Ok("svc2")
        }
    }

    #[derive(Clone)]
    struct Svc2Factory;
    impl ServiceFactory<(), &'static str> for Svc2Factory {
        type Response = &'static str;
        type Error = ();
        type Service = Svc2;
        type InitError = ();
        type Data = ();

        async fn create(&self, _: &'static str) -> Result<Self::Service, Self::InitError> {
            Ok(Svc2)
        }

        async fn map_data(&self, _: &&'static str, _: &Self::Data) -> Result<(), ()> {
            Ok(())
        }
    }

    type Either = EitherService<Svc1, Svc2>;
    type EitherFactory<F> = EitherServiceFactory<F, Svc1Factory, Svc2Factory, ()>;

    #[ntex::test]
    async fn test_success() {
        let svc = Pipeline::new(Either::left(Svc1).clone(), ());
        assert_eq!(svc.call(()).await, Ok("svc1"));
        assert_eq!(svc.ready().await, Ok(()));
        svc.shutdown().await;

        let svc = Pipeline::new(Either::right(Svc2).clone(), ());
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

        let svc = factory.pipeline("svc1", &()).await.unwrap();
        assert_eq!(svc.call(()).await, Ok("svc1"));

        let svc = factory.pipeline("other", &()).await.unwrap();
        assert_eq!(svc.call(()).await, Ok("svc2"));
    }
}
