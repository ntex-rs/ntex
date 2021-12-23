use std::{future::Future, net, pin::Pin};

use crate::http::body::Body;
use crate::http::RequestHeadType;
use crate::service::Service;

use super::error::{ConnectError, SendRequestError};
use super::response::ClientResponse;
use super::{Connect as ClientConnect, Connection};

pub(super) struct ConnectorWrapper<T>(pub(crate) T);

pub(super) trait Connect {
    fn send_request(
        &self,
        head: RequestHeadType,
        body: Body,
        addr: Option<net::SocketAddr>,
    ) -> Pin<Box<dyn Future<Output = Result<ClientResponse, SendRequestError>>>>;
}

impl<T> Connect for ConnectorWrapper<T>
where
    T: Service<ClientConnect, Response = Connection, Error = ConnectError>,
    T::Future: 'static,
{
    fn send_request(
        &self,
        head: RequestHeadType,
        body: Body,
        addr: Option<net::SocketAddr>,
    ) -> Pin<Box<dyn Future<Output = Result<ClientResponse, SendRequestError>>>> {
        // connect to the host
        let fut = self.0.call(ClientConnect {
            uri: head.as_ref().uri.clone(),
            addr,
        });

        Box::pin(async move {
            let connection = fut.await?;

            // send request
            connection
                .send_request(head, body)
                .await
                .map(|(head, payload)| ClientResponse::new(head, payload))
        })
    }
}
