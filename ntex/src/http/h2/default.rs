use std::{future::Future, io};

use ntex_h2 as h2;

use crate::service::{Service, ServiceCtx, ServiceFactory, cfg::SharedCfg};
use crate::{http::error::H2Error, util::Ready};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl ServiceFactory<h2::Control<H2Error>, SharedCfg> for DefaultControlService {
    type Response = h2::ControlAck;
    type Error = io::Error;
    type Service = DefaultControlService;
    type InitError = io::Error;

    fn create(
        &self,
        _: SharedCfg,
    ) -> impl Future<Output = Result<Self::Service, Self::InitError>> {
        Ready::Ok(DefaultControlService)
    }
}

impl Service<h2::Control<H2Error>> for DefaultControlService {
    type Response = h2::ControlAck;
    type Error = io::Error;

    fn call(
        &self,
        msg: h2::Control<H2Error>,
        _: ServiceCtx<'_, Self>,
    ) -> impl Future<Output = Result<Self::Response, Self::Error>> {
        log::trace!("HTTP/2 Control message: {msg:?}");
        Ready::Ok(msg.ack())
    }
}
