#[cfg(feature = "compress")]
use crate::http::{Payload, encoding::Decoder};
use crate::{Ctx, Service, error::Error, http::body::MessageBody};

use super::error::ClientError;
use super::{ClientConfig, ClientRawRequest, Connect, Connector, ServiceRequest, ServiceResponse};

#[derive(Debug)]
pub struct Sender {
    config: ClientConfig,
    connector: Connector,
}

impl Sender {
    pub(crate) fn new(connector: Connector, config: ClientConfig) -> Self {
        Self { config, connector }
    }
}

#[allow(unused_variables)]
impl Service<(), ServiceRequest> for Sender {
    type Res = ServiceResponse;
    type Error = Error<ClientError>;

    crate::forward_ready!((), connector);
    crate::forward_shutdown!((), connector);

    async fn call(
        &self,
        req: ServiceRequest,
        ctx: Ctx<'_, Self, ()>,
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

        if timeout.is_zero() {
            timeout = self.config.timeout();
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
                config: self.config.clone(),
            });
        }

        Ok(ServiceResponse {
            head,
            payload,
            config: self.config.clone(),
        })
    }
}
