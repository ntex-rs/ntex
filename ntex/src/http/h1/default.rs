use std::{convert::Infallible, io};

use crate::{Ctx, Service, ServiceFactory, http::ResponseError, io::Filter};

use super::control::{Control, ControlAck};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

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
        r: Control<F, Err>,
        _: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        Ok(r.ack())
    }
}

impl<St, F, Err, Cfg> ServiceFactory<St, Control<F, Err>, Cfg> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Res = ControlAck<F>;
    type Error = io::Error;

    type Service = DefaultControlService;
    type InitError = Infallible;

    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}
