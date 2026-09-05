use std::fmt;

use ntex_h2 as h2;

use crate::{Ctx, Service, ServiceFactory, http::error::DispatchError};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl<St, E: fmt::Debug> Service<St, h2::Control<E>> for DefaultControlService {
    type Res = h2::ControlAck;
    type Error = DispatchError;

    async fn call(
        &self,
        msg: h2::Control<E>,
        _: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        log::trace!("HTTP/2 Control message: {msg:?}");
        Ok(msg.ack())
    }
}

impl<St, E: fmt::Debug> ServiceFactory<St, h2::Control<E>> for DefaultControlService {
    type Res = h2::ControlAck;
    type Error = DispatchError;

    type Service = DefaultControlService;
    type InitError = DispatchError;

    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}
