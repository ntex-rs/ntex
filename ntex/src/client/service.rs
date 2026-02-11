use std::{error::Error, net, rc::Rc};

use serde::Serialize;

use crate::http::body::{Body, BodyStream};
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::{Message, Payload, RequestHead, ResponseHead, error::HttpError};
use crate::{time::Millis, util::Bytes, util::Stream};

use super::{ClientConfig, error::SendRequestError};

#[derive(Debug)]
pub struct ServiceResponse {
    pub(super) head: ResponseHead,
    pub(super) payload: Payload,
    pub(super) config: ClientConfig,
}

#[derive(Debug)]
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
    /// Mutable reference to a http message part of the request
    pub fn head(&mut self) -> &mut RequestHead {
        &mut self.head
    }

    /// Mutable reference to the request's extra headers.
    pub fn headers(&mut self) -> &mut HeaderMap {
        if self.headers.is_none() {
            self.headers = Some(HeaderMap::new());
        }
        self.headers.as_mut().unwrap()
    }

    /// Get request's body
    pub fn body(&mut self) -> &mut Body {
        &mut self.body
    }

    /// Get request's address
    pub fn address(&mut self) -> &mut Option<net::SocketAddr> {
        &mut self.addr
    }

    pub(super) fn set_json<T: Serialize>(
        &mut self,
        value: &T,
    ) -> Result<(), SendRequestError> {
        self.body = serde_json::to_string(value)
            .map_err(|e| SendRequestError::Error(Rc::new(e)))?
            .into();
        self.set_header_if_none(header::CONTENT_TYPE, "application/json")?;
        Ok(())
    }

    pub(super) fn set_form<T: Serialize>(
        &mut self,
        value: &T,
    ) -> Result<(), SendRequestError> {
        self.body = serde_urlencoded::to_string(value)
            .map_err(|e| SendRequestError::Error(Rc::new(e)))?
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
            match HeaderValue::try_from(value) {
                Ok(value) => self.head.headers.insert(key, value),
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }
}
