use std::io;

use ntex_h2 as h2;

use crate::http::error::H2Error;
use crate::service::{Ctx, Service, ServiceFactory, cfg::SharedCfg};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl<St> ServiceFactory<St, h2::Control<H2Error>> for DefaultControlService {
    type Res = h2::ControlAck;
    type Error = io::Error;

    type Service = DefaultControlService;
    type InitCfg = SharedCfg;
    type InitError = io::Error;

    async fn create(&self, _: &SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

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
