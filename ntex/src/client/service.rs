use std::{error::Error, net, rc::Rc};

use serde::Serialize;

use crate::http::body::{Body, BodyStream};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::{Message, Payload, RequestHead, ResponseHead, error::HttpError};
use crate::{Cfg, time::Millis, util::Bytes, util::Stream};

use super::{ClientConfig, error::ClientError};

#[derive(Debug)]
/// HTTP response passed through client middleware.
pub struct ServiceResponse {
    pub(super) head: ResponseHead,
    pub(super) payload: Payload,
    pub(super) config: Cfg<ClientConfig>,
}

impl ServiceResponse {
    /// Returns the response head.
    pub fn head(&self) -> &ResponseHead {
        &self.head
    }

    /// Returns mutable access to the response head.
    pub fn head_mut(&mut self) -> &mut ResponseHead {
        &mut self.head
    }

    /// Returns mutable access to the response payload.
    pub fn payload(&mut self) -> &mut Payload {
        &mut self.payload
    }

    /// Takes the response payload, leaving an empty payload behind.
    pub fn take_payload(&mut self) -> Payload {
        std::mem::replace(&mut self.payload, Payload::None)
    }
}

#[derive(Debug)]
/// HTTP request passed through client middleware before it is sent.
pub struct ServiceRequest {
    pub(super) head: Message<RequestHead>,
    pub(super) headers: Option<HeaderMap>,
    pub(super) addr: Option<net::SocketAddr>,
    pub(super) body: Body,
    pub(super) timeout: Millis,
    pub(super) response_decompress: bool,
}

impl ServiceRequest {
    pub(super) fn new() -> Self {
        Self {
            head: Message::new(),
            headers: None,
            addr: None,
            body: Body::None,
            timeout: Millis::ZERO,
            response_decompress: true,
        }
    }

    #[inline]
    /// Returns mutable access to the request head.
    pub fn head(&mut self) -> &mut RequestHead {
        &mut self.head
    }

    /// Returns mutable access to the request's extra headers.
    ///
    /// Extra headers override headers with the same name in the request head
    /// when the request is encoded.
    pub fn headers(&mut self) -> &mut HeaderMap {
        if self.headers.is_none() {
            self.headers = Some(HeaderMap::new());
        }
        self.headers.as_mut().unwrap()
    }

    /// Returns mutable access to the request body.
    pub fn body(&mut self) -> &mut Body {
        &mut self.body
    }

    /// Returns mutable access to the pre-resolved server address.
    pub fn address(&mut self) -> &mut Option<net::SocketAddr> {
        &mut self.addr
    }

    pub(super) fn set_json<T: Serialize>(&mut self, value: &T) -> Result<(), ClientError> {
        self.body = serde_json::to_string(value)
            .map_err(|e| ClientError::Error(Rc::new(e)))?
            .into();
        self.set_header_if_none(header::CONTENT_TYPE, "application/json")?;
        Ok(())
    }

    pub(super) fn set_form<T: Serialize>(&mut self, value: &T) -> Result<(), ClientError> {
        self.body = serde_urlencoded::to_string(value)
            .map_err(|e| ClientError::Error(Rc::new(e)))?
            .into();
        self.set_header_if_none(header::CONTENT_TYPE, "application/x-www-form-urlencoded")?;
        Ok(())
    }

    pub(super) fn set_stream<S, E>(&mut self, stream: S)
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        self.body = BodyStream::new(stream).into();
    }

    fn set_header_if_none<V>(&mut self, key: HeaderName, value: V) -> Result<(), HttpError>
    where
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if !self.head.headers.contains_key(&key) {
            self.head
                .headers
                .insert(key, HeaderValue::try_from(value).map_err(Into::into)?);
        }

        Ok(())
    }
}
