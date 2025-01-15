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

impl<R, C, ChooseFn, SFLeft, SFRight> ServiceFactory<R, C>
    for EitherServiceFactory<ChooseFn, SFLeft, SFRight>
where
    ChooseFn: Fn(&C) -> bool,
    SFLeft: ServiceFactory<R, C>,
    SFRight: ServiceFactory<
        R,
        C,
        Response = SFLeft::Response,
        InitError = SFLeft::InitError,
        Error = SFLeft::Error,
    >,
{
    type Response = SFLeft::Response;
    type Error = SFLeft::Error;
    type InitError = SFLeft::InitError;
    type Service = EitherService<SFLeft::Service, SFRight::Service>;

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
    SRight: Service<Req, Response = SLeft::Response, Error = SLeft::Error>,
{
    type Response = SLeft::Response;
    type Error = SLeft::Error;

    #[inline]
    async fn ready(&self, ctx: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
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
    async fn call(
        &self,
        req: Req,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        match self.svc {
            Either::Left(ref svc) => ctx.call(svc, req).await,
            Either::Right(ref svc) => ctx.call(svc, req).await,
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
