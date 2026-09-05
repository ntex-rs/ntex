use std::{convert::Infallible, io};

use ntex_h2 as h2;

use crate::{Ctx, Service, ServiceFactory, http::error::H2Error};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl<St> Service<St, h2::Control<H2Error>> for DefaultControlService {
    type Res = h2::ControlAck;
    type Error = io::Error;

    async fn call(
        &self,
        msg: h2::Control<H2Error>,
        _: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        log::trace!("HTTP/2 Control message: {msg:?}");
        Ok(msg.ack())
    }
}

impl<St> ServiceFactory<St, h2::Control<H2Error>> for DefaultControlService {
    type Res = h2::ControlAck;
    type Error = io::Error;

    type Service = DefaultControlService;
    type InitError = Infallible;

    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}
