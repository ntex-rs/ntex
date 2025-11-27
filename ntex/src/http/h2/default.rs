use std::io;

use ntex_h2 as h2;

use crate::http::error::H2Error;
use crate::service::{Service, ServiceCtx, ServiceFactory, cfg::SharedCfg};

#[derive(Default)]
/// Default control service
pub struct DefaultControlService;

impl ServiceFactory<h2::Control<H2Error>, SharedCfg> for DefaultControlService {
    type Response = h2::ControlAck;
    type Error = io::Error;
    type Service = DefaultControlService;
    type InitError = io::Error;

    async fn create(&self, _: SharedCfg) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

impl Service<h2::Control<H2Error>> for DefaultControlService {
    type Response = h2::ControlAck;
    type Error = io::Error;

    async fn call(
        &self,
        msg: h2::Control<H2Error>,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        log::trace!("HTTP/2 Control message: {msg:?}");
        Ok(msg.ack())
    }
}
