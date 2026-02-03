use std::{error::Error, fmt, net, rc::Rc};

use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::http::{Method, RequestHead, RequestHeadType, Uri, body::Body};
use crate::{time::Millis, util::Bytes, util::Stream};

use super::{ClientInner, ClientResponse, error::SendRequestError};

/// `FrozenClientRequest` struct represents clonable client request.
/// It could be used to send same request multiple times.
#[derive(Clone)]
pub struct FrozenClientRequest {
    pub(super) head: Rc<RequestHead>,
    pub(super) addr: Option<net::SocketAddr>,
    pub(super) response_decompress: bool,
    pub(super) timeout: Millis,
    pub(super) config: Rc<ClientInner>,
}

impl FrozenClientRequest {
    /// Get HTTP URI of request
    pub fn get_uri(&self) -> &Uri {
        &self.head.uri
    }

    /// Get HTTP method of this request
    pub fn get_method(&self) -> &Method {
        &self.head.method
    }

    /// Returns request's headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.head.headers
    }

    /// Send a body.
    pub async fn send_body<B>(&self, body: B) -> Result<ClientResponse, SendRequestError>
    where
        B: Into<Body>,
    {
        RequestHeadType::Rc(self.head.clone(), None)
            .send_body(
                self.addr,
                self.response_decompress,
                self.timeout,
                self.config.clone(),
                body,
            )
            .await
    }

    /// Send a json body.
    pub async fn send_json<T: serde::Serialize>(
        &self,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        RequestHeadType::Rc(self.head.clone(), None)
            .send_json(
                self.addr,
                self.response_decompress,
                self.timeout,
                self.config.clone(),
                value,
            )
            .await
    }

    /// Send an urlencoded body.
    pub async fn send_form<T: serde::Serialize>(
        &self,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        RequestHeadType::Rc(self.head.clone(), None)
            .send_form(
                self.addr,
                self.response_decompress,
                self.timeout,
                self.config.clone(),
                value,
            )
            .await
    }

    /// Send a streaming body.
    pub async fn send_stream<S, E>(
        &self,
        stream: S,
    ) -> Result<ClientResponse, SendRequestError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        RequestHeadType::Rc(self.head.clone(), None)
            .send_stream(
                self.addr,
                self.response_decompress,
                self.timeout,
                self.config.clone(),
                stream,
            )
            .await
    }

    /// Send an empty body.
    pub async fn send(&self) -> Result<ClientResponse, SendRequestError> {
        RequestHeadType::Rc(self.head.clone(), None)
            .send(
                self.addr,
                self.response_decompress,
                self.timeout,
                self.config.clone(),
            )
            .await
    }

    /// Create a `FrozenSendBuilder` with extra headers
    pub fn extra_headers(&self, extra_headers: HeaderMap) -> FrozenSendBuilder {
        FrozenSendBuilder::new(self.clone(), extra_headers)
    }

    /// Create a `FrozenSendBuilder` with an extra header
    pub fn extra_header<K, V>(&self, key: K, value: V) -> FrozenSendBuilder
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        self.extra_headers(HeaderMap::new())
            .extra_header(key, value)
    }
}

impl fmt::Debug for FrozenClientRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nFrozenClientRequest {:?} {}:{}",
            self.head.version, self.head.method, self.head.uri
        )?;
        writeln!(f, "  headers:")?;
        for (key, val) in &self.head.headers {
            if key == header::AUTHORIZATION {
                writeln!(f, "    {key:?}: <REDACTED>")?;
            } else {
                writeln!(f, "    {key:?}: {val:?}")?;
            }
        }
        Ok(())
    }
}

/// Builder that allows to modify extra headers.
#[derive(Debug)]
pub struct FrozenSendBuilder {
    req: FrozenClientRequest,
    extra_headers: HeaderMap,
    err: Option<HttpError>,
}

impl FrozenSendBuilder {
    pub(crate) fn new(req: FrozenClientRequest, extra_headers: HeaderMap) -> Self {
        Self {
            req,
            extra_headers,
            err: None,
        }
    }

    /// Insert a header, it overrides existing header in `FrozenClientRequest`.
    pub fn extra_header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => self.extra_headers.insert(key, value),
                Err(e) => self.err = Some(e.into()),
            },
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Complete request construction and send a body.
    pub async fn send_body<B>(self, body: B) -> Result<ClientResponse, SendRequestError>
    where
        B: Into<Body>,
    {
        if let Some(e) = self.err {
            return Err(e.into());
        }

        RequestHeadType::Rc(self.req.head, Some(self.extra_headers))
            .send_body(
                self.req.addr,
                self.req.response_decompress,
                self.req.timeout,
                self.req.config,
                body,
            )
            .await
    }

    /// Complete request construction and send a json body.
    pub async fn send_json<T: serde::Serialize>(
        self,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        if let Some(e) = self.err {
            return Err(e.into());
        }

        RequestHeadType::Rc(self.req.head, Some(self.extra_headers))
            .send_json(
                self.req.addr,
                self.req.response_decompress,
                self.req.timeout,
                self.req.config,
                value,
            )
            .await
    }

    /// Complete request construction and send an urlencoded body.
    pub async fn send_form<T: serde::Serialize>(
        self,
        value: &T,
    ) -> Result<ClientResponse, SendRequestError> {
        if let Some(e) = self.err {
            return Err(e.into());
        }

        RequestHeadType::Rc(self.req.head, Some(self.extra_headers))
            .send_form(
                self.req.addr,
                self.req.response_decompress,
                self.req.timeout,
                self.req.config,
                value,
            )
            .await
    }

    /// Complete request construction and send a streaming body.
    pub async fn send_stream<S, E>(
        self,
        stream: S,
    ) -> Result<ClientResponse, SendRequestError>
    where
        S: Stream<Item = Result<Bytes, E>> + Unpin + 'static,
        E: Error + 'static,
    {
        if let Some(e) = self.err {
            return Err(e.into());
        }

        RequestHeadType::Rc(self.req.head, Some(self.extra_headers))
            .send_stream(
                self.req.addr,
                self.req.response_decompress,
                self.req.timeout,
                self.req.config,
                stream,
            )
            .await
    }

    /// Complete request construction and send an empty body.
    pub async fn send(self) -> Result<ClientResponse, SendRequestError> {
        if let Some(e) = self.err {
            return Err(e.into());
        }

        RequestHeadType::Rc(self.req.head, Some(self.extra_headers))
            .send(
                self.req.addr,
                self.req.response_decompress,
                self.req.timeout,
                self.req.config,
            )
            .await
    }
}
