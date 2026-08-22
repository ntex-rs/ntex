#[cfg(feature = "compress")]
use crate::http::{Payload, encoding::Decoder};
use crate::{Ctx, Service, SharedCfg, error::Error, http::body::MessageBody};

use super::{ClientConfig, ClientRawRequest, Connect, ServiceRequest, ServiceResponse};
use super::{connector::Connector, error::ClientError};

#[derive(Debug)]
pub struct Sender {
    connector: Connector,
}

impl Sender {
    pub(super) fn new(connector: Connector) -> Self {
        Self { connector }
    }
}

#[allow(unused_variables)]
impl Service<SharedCfg, ServiceRequest> for Sender {
    type Res = ServiceResponse;
    type Error = Error<ClientError>;

    crate::forward_ready!(SharedCfg, connector);
    crate::forward_shutdown!(SharedCfg, connector);

    async fn call(
        &self,
        req: ServiceRequest,
        ctx: Ctx<'_, Self, SharedCfg>,
    ) -> Result<Self::Res, Self::Error> {
        let ServiceRequest {
            head,
            addr,
            body,
            headers,
            mut timeout,
            response_decompress,
        } = req;

        let uri = head.uri.clone();
        let con = ctx.call(&self.connector, Connect { uri, addr }).await?;
        let config = ctx.st().get::<ClientConfig>();

        if timeout.is_zero() {
            timeout = config.timeout();
        }

        let req = ClientRawRequest {
            head,
            headers,
            size: body.size(),
        };

        let (head, payload) = con.send_request(req, body, timeout).await?;

        #[cfg(feature = "compress")]
        if response_decompress {
            let payload = Payload::from_stream(Decoder::from_headers(payload, &head.headers));
            return Ok(ServiceResponse {
                head,
                payload,
                config: config.clone(),
            });
        }

        Ok(ServiceResponse {
            head,
            payload,
            config,
        })
    }
}
