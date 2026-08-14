#![allow(clippy::unused_async_trait_impl)]
use std::io;

use crate::http::ResponseError;
use crate::io::Filter;
use crate::service::{Service, ServiceCtx, cfg::SharedCfg};

use super::control::{Control, ControlAck};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl Service<SharedCfg> for DefaultControlService {
    type Response = DefaultControlService;
    type Error = io::Error;
    type Data = ();

    #[inline]
    async fn call(
        &self,
        _: SharedCfg,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(DefaultControlService)
    }
}

impl<F, Err> Service<Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Response = ControlAck<F>;
    type Error = io::Error;
    type Data = ();

    #[inline]
    async fn call(
        &self,
        req: Control<F, Err>,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(req.ack())
    }
}
