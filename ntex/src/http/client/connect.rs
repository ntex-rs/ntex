use std::{fmt, future::Future, io, net, pin::Pin, task::Context, task::Poll};

use crate::http::body::Body;
use crate::http::h1::ClientCodec;
use crate::http::{RequestHeadType, ResponseHead};
use crate::io::IoBoxed;
use crate::Service;

use super::error::{ConnectError, SendRequestError};
use super::response::ClientResponse;
use super::{Connect as ClientConnect, Connection};

pub(crate) type TunnelFuture = Pin<
    Box<
        dyn Future<
            Output = Result<(ResponseHead, IoBoxed, ClientCodec), SendRequestError>,
        >,
    >,
>;

pub(super) struct ConnectorWrapper<T>(pub(crate) T);

pub(super) trait Connect {
    fn send_request(
        &self,
        head: RequestHeadType,
        body: Body,
        addr: Option<net::SocketAddr>,
    ) -> Pin<Box<dyn Future<Output = Result<ClientResponse, SendRequestError>>>>;

    /// Send request, returns Response and Framed
    fn open_tunnel(
        &self,
        head: RequestHeadType,
        addr: Option<net::SocketAddr>,
    ) -> TunnelFuture;
}

impl<T> Connect for ConnectorWrapper<T>
where
    T: Service<Request = ClientConnect, Response = Connection, Error = ConnectError>,
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

    fn open_tunnel(
        &self,
        head: RequestHeadType,
        addr: Option<net::SocketAddr>,
    ) -> TunnelFuture {
        // connect to the host
        let fut = self.0.call(ClientConnect {
            uri: head.as_ref().uri.clone(),
            addr,
        });

        Box::pin(async move {
            let connection = fut.await?;

            // send request
            connection.open_tunnel(head).await
        })
    }
}
