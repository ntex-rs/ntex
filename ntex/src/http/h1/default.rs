use std::{error::Error, rc::Rc};

use crate::{Ctx, Service, http::ResponseError, io::Filter};

use super::control::{Control, ControlAck};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService;

impl<F, Err> Service<(), Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Res = ControlAck<F>;
    type Error = Rc<dyn Error>;

    #[inline]
    async fn call(
        &self,
        r: Control<F, Err>,
        _: Ctx<'_, Self, ()>,
    ) -> Result<Self::Res, Self::Error> {
        Ok(r.ack())
    }
}
