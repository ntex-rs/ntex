#![allow(clippy::unused_async_trait_impl)]
use std::io;

use crate::http::ResponseError;
use crate::io::Filter;
use crate::service::{Ctx, Service, ServiceFactory, cfg::SharedCfg};

use super::control::{Control, ControlAck};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl<St, F, Err> ServiceFactory<St, Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Res = ControlAck<F>;
    type Error = io::Error;
    type Service = DefaultControlService;
    type InitCfg = SharedCfg;
    type InitError = io::Error;

    #[inline]
    async fn create(&self, _: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

impl<St, F, Err> Service<St, Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Res = ControlAck<F>;
    type Error = io::Error;

    #[inline]
    async fn call(
        &self,
        req: Control<F, Err>,
        _: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        Ok(req.ack())
    }
}
