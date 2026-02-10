use crate::{Service, ServiceCtx};

#[cfg(feature = "compress")]
use crate::http::Payload;
#[cfg(feature = "compress")]
use crate::http::encoding::Decoder;

use crate::http::body::MessageBody;

use super::connector::ConnectorService;
use super::error::SendRequestError;
use super::{ClientConfig, ClientRawRequest, Connect, ServiceRequest, ServiceResponse};

#[derive(Debug)]
pub struct Sender {
    config: ClientConfig,
    connector: ConnectorService,
}

impl Sender {
    pub(crate) fn new(connector: ConnectorService, config: ClientConfig) -> Self {
        Self { config, connector }
    }
}

#[allow(unused_variables)]
impl Service<ServiceRequest> for Sender {
    type Response = ServiceResponse;
    type Error = SendRequestError;

    crate::forward_ready!(connector);
    crate::forward_poll!(connector);
    crate::forward_shutdown!(connector);

    async fn call(
        &self,
        req: ServiceRequest,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        let ServiceRequest {
            head,
            addr,
            body,
            headers,
            mut timeout,
            response_decompress,
        } = req;

        let con = ctx
            .call(
                &self.connector,
                Connect {
                    addr,
                    uri: head.uri.clone(),
                },
            )
            .await?;

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
            let payload =
                Payload::from_stream(Decoder::from_headers(payload, &head.headers));
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
