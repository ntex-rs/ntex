use std::io;

use crate::http::ResponseError;
use crate::io::Filter;
use crate::service::{Service, ServiceCtx, ServiceFactory};

use super::control::{Control, ControlAck};

#[derive(Default)]
/// Default control service
pub struct DefaultControlService;

impl<F, Err> ServiceFactory<Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Response = ControlAck;
    type Error = io::Error;
    type Service = DefaultControlService;
    type InitError = io::Error;

    #[inline]
    async fn create(&self, _: ()) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

impl<F, Err> Service<Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Response = ControlAck;
    type Error = io::Error;

    #[inline]
    async fn call(
        &self,
        req: Control<F, Err>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(req.ack())
    }
}
