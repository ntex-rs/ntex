use std::{fmt, net, rc::Rc};

use crate::http::{body::Body, RequestHeadType};
use crate::{service::Pipeline, service::Service, time::Millis, util::BoxFuture};

use super::error::{ConnectError, SendRequestError};
use super::response::ClientResponse;
use super::{ClientConfig, Connect as ClientConnect, Connection};

pub(super) struct ConnectorWrapper<T>(pub(crate) Pipeline<T>);

impl<T> fmt::Debug for ConnectorWrapper<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connector")
            .field("service", &self.0)
            .finish()
    }
}

#[doc(hidden)]
pub trait Connect: fmt::Debug {
    fn send_request(
        &self,
        head: RequestHeadType,
        body: Body,
        addr: Option<net::SocketAddr>,
        timeout: Millis,
        cfg: Rc<ClientConfig>,
    ) -> BoxFuture<'_, Result<ClientResponse, SendRequestError>>;
}

impl<T> Connect for ConnectorWrapper<T>
where
    T: Service<ClientConnect, Response = Connection, Error = ConnectError> + fmt::Debug,
{
    fn send_request(
        &self,
        head: RequestHeadType,
        body: Body,
        addr: Option<net::SocketAddr>,
        timeout: Millis,
        cfg: Rc<ClientConfig>,
    ) -> BoxFuture<'_, Result<ClientResponse, SendRequestError>> {
        Box::pin(async move {
            // connect to the host
            let fut = self.0.call(ClientConnect {
                uri: head.as_ref().uri.clone(),
                addr,
            });

            let connection = fut.await?;

            // send request
            connection
                .send_request(head, body, timeout)
                .await
                .map(|(head, payload)| ClientResponse::new(head, payload, cfg))
        })
    }
}
