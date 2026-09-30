use crate::http::{ResponseError, error::DispatchError};
use crate::{Ctx, Service, ServiceFactory, io::Filter};

use super::control::{Control, ControlAck};

#[derive(Debug, Default)]
/// Control service that acknowledges every HTTP/1 lifecycle event using its
/// default action.
pub struct DefaultControlService;

impl<St, F, Err> Service<St, Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Res = ControlAck<F>;
    type Error = DispatchError;

    #[inline]
    async fn call(
        &self,
        r: Control<F, Err>,
        _: Ctx<'_, Self, St>,
    ) -> Result<Self::Res, Self::Error> {
        Ok(r.ack())
    }
}

impl<St, F, Err> ServiceFactory<St, Control<F, Err>> for DefaultControlService
where
    F: Filter,
    Err: ResponseError,
{
    type Res = ControlAck<F>;
    type Error = DispatchError;

    type Service = DefaultControlService;
    type InitError = DispatchError;

    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(DefaultControlService)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::http::{Request, h1::control::ControlResult};
    use crate::{Pipeline, io::Base};

    #[crate::rt_test]
    async fn default_acks() {
        let svc: Pipeline<Control<Base, io::Error>, _, _> =
            ServiceFactory::pipeline(&DefaultControlService, ())
                .await
                .unwrap();

        let ack = svc
            .call(Control::<Base, io::Error>::request(Request::new()))
            .await
            .unwrap();
        assert!(matches!(ack.result, ControlResult::Publish(_)));

        let ack = svc
            .call(Control::<Base, io::Error>::keepalive(false))
            .await
            .unwrap();
        assert!(matches!(ack.result, ControlResult::Stop));
    }
}
