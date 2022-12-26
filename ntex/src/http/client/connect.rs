use std::net;

use crate::http::{body::Body, RequestHeadType};
use crate::{service::Service, util::BoxFuture};

use super::error::{ConnectError, SendRequestError};
use super::response::ClientResponse;
use super::{Connect as ClientConnect, Connection};

pub(super) struct ConnectorWrapper<T>(pub(crate) T);

pub(super) trait Connect {
    fn send_request<'a>(
        &'a self,
        head: RequestHeadType,
        body: Body,
        addr: Option<net::SocketAddr>,
    ) -> BoxFuture<'a, Result<ClientResponse, SendRequestError>>;
}

impl<T> Connect for ConnectorWrapper<T>
where
    T: Service<ClientConnect, Response = Connection, Error = ConnectError>,
{
    fn send_request<'a>(
        &'a self,
        head: RequestHeadType,
        body: Body,
        addr: Option<net::SocketAddr>,
    ) -> BoxFuture<'a, Result<ClientResponse, SendRequestError>> {
        Box::pin(async move {
            // connect to the host
            let fut = self.0.call(ClientConnect {
                uri: head.as_ref().uri.clone(),
                addr,
            });

            let connection = fut.await?;

            // send request
            connection
                .send_request(head, body)
                .await
                .map(|(head, payload)| ClientResponse::new(head, payload))
        })
    }
}
