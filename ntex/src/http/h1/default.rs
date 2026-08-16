use std::{io, marker::PhantomData};

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

impl<F, Err> Service for DefaultControlService<F, Err>
where
    F: Filter,
    Err: ResponseError,
{
    type St = ();
    type Req = Control<F, Err>;
    type Res = ControlAck<F>;
    type Error = io::Error;

    #[inline]
    async fn call(&self, r: Self::Req, _: Ctx<'_, Self>) -> Result<Self::Res, io::Error> {
        Ok(r.ack())
    }
}
