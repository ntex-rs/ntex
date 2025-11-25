use std::{error::Error, net, rc::Rc};

use serde::Serialize;

use crate::http::RequestHeadType;
use crate::http::body::{Body, BodyStream};
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::time::Millis;
use crate::util::{Bytes, Stream};

#[cfg(feature = "compress")]
use crate::http::Payload;
#[cfg(feature = "compress")]
use crate::http::encoding::Decoder;

use super::error::{FreezeRequestError, InvalidUrl, SendRequestError};
use super::{ClientInner, ClientResponse, Connect};

#[derive(thiserror::Error, Clone, Debug)]
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

impl RequestHeadType {
    pub(super) async fn send_body<B>(
        self,
        addr: Option<net::SocketAddr>,
        _response_decompress: bool,
        mut timeout: Millis,
        config: Rc<ClientInner>,
        body: B,
    ) -> Result<ClientResponse, SendRequestError>
    where
        B: Into<Body>,
    {
        let con = config
            .connector
            .call(Connect {
                addr,
                uri: self.as_ref().uri.clone(),
            })
            .await?;

        if timeout.is_zero() {
            timeout = config.config.timeout;
        }
        #[allow(unused_mut)]
        let mut res =
            con.send_request(self, body.into(), timeout)
                .await
                .map(|(head, payload)| {
                    ClientResponse::new(head, payload, config.config.clone())
                })?;

        #[cfg(feature = "compress")]
        if _response_decompress {
            let payload = res.take_payload();
            res.set_payload(Payload::from_stream(Decoder::from_headers(
                payload,
                &res.head.headers,
            )))
        }
        Ok(res)
    }

    pub(super) async fn send_json<T: Serialize>(
        mut self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientInner>,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        let body = serde_json::to_string(value)
            .map_err(|e| SendRequestError::Error(Rc::new(e)))?;
        self.set_header_if_none(header::CONTENT_TYPE, "application/json")?;

        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::Bytes(Bytes::from(body)),
        )
        .await
    }

    pub(super) async fn send_form<T: Serialize>(
        mut self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientInner>,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        let body = serde_urlencoded::to_string(value)
            .map_err(|e| SendRequestError::Error(Rc::new(e)))?;

        // set content-type
        self.set_header_if_none(header::CONTENT_TYPE, "application/x-www-form-urlencoded")?;

        self.send_body(
            addr,
            response_decompress,
            timeout,
            config,
            Body::Bytes(Bytes::from(body)),
        )
        .await
    }

    pub(super) async fn send_stream<S, E>(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientInner>,
        stream: S,
    ) -> Result<ClientResponse, SendRequestError>
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
        .await
    }

    pub(super) async fn send(
        self,
        addr: Option<net::SocketAddr>,
        response_decompress: bool,
        timeout: Millis,
        config: Rc<ClientInner>,
    ) -> Result<ClientResponse, SendRequestError> {
        self.send_body(addr, response_decompress, timeout, config, Body::None)
            .await
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
