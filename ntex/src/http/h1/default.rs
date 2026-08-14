use std::{io, marker::PhantomData};

use crate::http::ResponseError;
use crate::io::Filter;
use crate::service::{Ctx, Service, ServiceFactory, cfg::SharedCfg};

use super::control::{Control, ControlAck};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService<F, Err>(PhantomData<(F, Err)>);

impl<F, Err> DefaultControlService<F, Err> {
    pub(crate) fn new() -> Self {
        DefaultControlService(PhantomData)
    }
}

impl<F, Err> ServiceFactory<Control<F, Err>> for DefaultControlService<F, Err>
where
    F: Filter,
    Err: ResponseError,
{
    type St = ();
    type Res = ControlAck<F>;
    type Error = io::Error;

    type Service = DefaultControlService<F, Err>;
    type InitCfg = SharedCfg;
    type InitError = io::Error;

    #[inline]
    async fn create(&self, _: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService::new())
    }
}

impl<F, Err> Service for DefaultControlService<F, Err>
where
    F: Filter,
    Err: ResponseError,
{
    type St = ();
    type Req = Control<F, Err>;
    type Res = ControlAck<F>;
    type Error = io::Error;

    #[inline]
    async fn call(&self, r: Self::Req, _: Ctx<'_, Self>) -> Result<Self::Res, io::Error> {
        Ok(r.ack())
    }
}
