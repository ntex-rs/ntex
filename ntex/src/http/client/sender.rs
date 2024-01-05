use std::task::{Context, Poll};
use std::{error::Error, future::Future, net, pin::Pin, rc::Rc};

use serde::Serialize;

use crate::http::body::{Body, BodyStream};
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::RequestHeadType;
use crate::time::Millis;
use crate::util::{BoxFuture, Bytes, Stream};

#[cfg(feature = "compress")]
use crate::http::encoding::Decoder;
#[cfg(feature = "compress")]
use crate::http::Payload;

use super::error::{FreezeRequestError, InvalidUrl, SendRequestError};
use super::response::ClientResponse;
use super::ClientConfig;

#[derive(thiserror::Error, Debug)]
pub(crate) enum PrepForSendingError {
    #[error("Invalid url {0}")]
    Url(#[from] InvalidUrl),
    #[error("Invalid http request {0}")]
    Http(#[from] HttpError),
}

impl From<PrepForSendingError> for FreezeRequestError {
    fn from(err: PrepForSendingError) -> FreezeRequestError {
        match err {
            PrepForSendingError::Url(e) => FreezeRequestError::Url(e),
            PrepForSendingError::Http(e) => FreezeRequestError::Http(e),
        }
    }
}

impl From<PrepForSendingError> for SendRequestError {
    fn from(err: PrepForSendingError) -> SendRequestError {
        match err {
            PrepForSendingError::Url(e) => SendRequestError::Url(e),
            PrepForSendingError::Http(e) => SendRequestError::Http(e),
        }
    }
}

/// Future that sends request's payload and resolves to a server response.
#[must_use = "futures do nothing unless polled"]
pub enum SendClientRequest {
    Fut(
        BoxFuture<'static, Result<ClientResponse, SendRequestError>>,
        bool,
    ),
    Err(Option<SendRequestError>),
}

impl SendClientRequest {
    pub(crate) fn new(
        send: BoxFuture<'static, Result<ClientResponse, SendRequestError>>,
        response_decompress: bool,
    ) -> SendClientRequest {
        SendClientRequest::Fut(send, response_decompress)
    }
}

impl Future for SendClientRequest {
    type Output = Result<ClientResponse, SendRequestError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match this {
            SendClientRequest::Fut(send, _response_decompress) => {
                let res = match Pin::new(send).poll(cx) {
                    Poll::Ready(res) => res,
                    Poll::Pending => return Poll::Pending,
                };

                #[cfg(feature = "compress")]
                let res = res.map(|mut res| {
                    if *_response_decompress {
                        let payload = res.take_payload();
                        res.set_payload(Payload::from_stream(Decoder::from_headers(
                            payload,
                            &res.head.headers,
                        )))
                    }
                    res
                });

                Poll::Ready(res)
            }
            SendClientRequest::Err(ref mut e) => match e.take() {
                Some(e) => Poll::Ready(Err(e)),
                None => panic!("Attempting to call completed future"),
            },
        }
    }
}

impl From<SendRequestError> for SendClientRequest {
    fn from(e: SendRequestError) -> Self {
        SendClientRequest::Err(Some(e))
    }
}

impl From<HttpError> for SendClientRequest {
    fn from(e: HttpError) -> Self {
        SendClientRequest::Err(Some(e.into()))
    }
}

impl From<PrepForSendingError> for SendClientRequest {
    fn from(e: PrepForSendingError) -> Self {
        SendClientRequest::Err(Some(e.into()))
    }
}

impl RequestHeadType {
    pub(super) fn send_body<B>(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        mut timeout: Millis,
        config: Rc<ClientConfig>,
        body: B,
    ) -> SendClientRequest
    where
        B: Into<Body>,
    {
        if timeout.is_zero() {
            timeout = config.timeout;
        }
        let body = body.into();

        let fut = Box::pin(async move {
            config
                .clone()
                .connector
                .send_request(self, body, addr, timeout, config)
                .await
        });

        SendClientRequest::new(fut, response_decompress)
    }

    pub(super) fn send_json<T: Serialize>(
        mut self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientConfig>,
        value: &T,
    ) -> SendClientRequest {
        let body = match serde_json::to_string(value) {
            Ok(body) => body,
            Err(e) => return SendRequestError::Error(Box::new(e)).into(),
        };

        if let Err(e) = self.set_header_if_none(header::CONTENT_TYPE, "application/json") {
            return e.into();
        }

        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::Bytes(Bytes::from(body)),
        )
    }

    pub(super) fn send_form<T: Serialize>(
        mut self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientConfig>,
        value: &T,
    ) -> SendClientRequest {
        let body = match serde_urlencoded::to_string(value) {
            Ok(body) => body,
            Err(e) => return SendRequestError::Error(Box::new(e)).into(),
        };

        // set content-type
        if let Err(e) = self
            .set_header_if_none(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        {
            return e.into();
        }

        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::Bytes(Bytes::from(body)),
        )
    }

    pub(super) fn send_stream<S, E>(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientConfig>,
        stream: S,
    ) -> SendClientRequest
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::from_message(BodyStream::new(stream)),
        )
    }

    pub(super) fn send(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientConfig>,
    ) -> SendClientRequest {
        self.send_body(addr, response_decompress, timeout, config, Body::None)
    }

    fn set_header_if_none<V>(&mut self, key: HeaderName, value: V) -> Result<(), HttpError>
    where
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match self {
            RequestHeadType::Owned(head) => {
                if !head.headers.contains_key(&key) {
                    match HeaderValue::try_from(value) {
                        Ok(value) => head.headers.insert(key, value),
                        Err(e) => return Err(e.into()),
                    }
                }
            }
            RequestHeadType::Rc(head, extra_headers) => {
                if !head.headers.contains_key(&key)
                    && !extra_headers.iter().any(|h| h.contains_key(&key))
                {
                    match HeaderValue::try_from(value) {
                        Ok(v) => {
                            let h = extra_headers.get_or_insert(HeaderMap::new());
                            h.insert(key, v)
                        }
                        Err(e) => return Err(e.into()),
                    };
                }
            }
        }

        Ok(())
    }
}
