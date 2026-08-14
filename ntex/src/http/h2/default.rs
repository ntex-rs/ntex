use std::io;

use ntex_h2 as h2;

use crate::http::error::H2Error;
use crate::service::{Service, ServiceCtx, cfg::SharedCfg};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl Service<SharedCfg> for DefaultControlService {
    type Response = DefaultControlService;
    type Error = io::Error;
    type Data = ();

    async fn call(
        &self,
        _: SharedCfg,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        Ok(DefaultControlService)
    }
}

impl Service<h2::Control<H2Error>> for DefaultControlService {
    type Response = h2::ControlAck;
    type Error = io::Error;
    type Data = ();

    async fn call(
        &self,
        msg: h2::Control<H2Error>,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        log::trace!("HTTP/2 Control message: {msg:?}");
        Ok(msg.ack())
    }
}
