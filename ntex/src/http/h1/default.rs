use std::{error::Error, marker::PhantomData, rc::Rc};

use crate::{Ctx, Service, http::ResponseError, io::Filter};

use super::control::{Control, ControlAck};

#[derive(Debug, Default)]
/// Default control service
pub struct DefaultControlService<F, Err>(PhantomData<(F, Err)>);

impl<F, Err> DefaultControlService<F, Err> {
    pub(crate) fn new() -> Self {
        DefaultControlService(PhantomData)
    }
}

impl<F, Err> Service<()> for DefaultControlService<F, Err>
where
    F: Filter,
    Err: ResponseError,
{
    type Req = Control<F, Err>;
    type Res = ControlAck<F>;
    type Error = Rc<dyn Error>;

    #[inline]
    async fn call(&self, r: Self::Req, _: Ctx<'_, Self, ()>) -> Result<Self::Res, Self::Error> {
        Ok(r.ack())
    }
}
