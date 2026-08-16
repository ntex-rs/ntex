use ntex_h2 as h2;

use crate::{Ctx, Service, http::error::H2Error};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl Service for DefaultControlService {
    type St = ();
    type Req = h2::Control<H2Error>;
    type Res = h2::ControlAck;
    type Error = std::io::Error;

    async fn call(
        &self,
        msg: h2::Control<H2Error>,
        _: Ctx<'_, Self>,
    ) -> Result<Self::Res, Self::Error> {
        log::trace!("HTTP/2 Control message: {msg:?}");
        Ok(msg.ack())
    }
}
