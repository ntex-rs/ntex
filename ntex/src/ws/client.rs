//! Websockets client
use std::{cell::RefCell, fmt, marker, net, rc::Rc, str};

#[cfg(feature = "openssl")]
use crate::connect::openssl;
#[cfg(feature = "rustls")]
use crate::connect::rustls;
#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use base64::{Engine, engine::general_purpose::STANDARD as base64};
use nanorand::{Rng, WyRand};

use crate::connect::{Connect, ConnectError, Connector};
use crate::http::header::{self, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use crate::http::{ConnectionType, RequestHead, RequestHeadType, StatusCode, Uri};
use crate::http::{body::BodySize, client, client::ClientResponse, error::HttpError, h1};
use crate::io::{Base, DispatchItem, Dispatcher, Filter, Io, Layer, Sealed, SharedConfig};
use crate::service::{
    IntoService, Pipeline, Service, ServiceFactory, apply_fn, fn_service,
};
use crate::time::{Millis, timeout};
use crate::{channel::mpsc, rt, util::Ready, ws};

use super::error::{WsClientBuilderError, WsClientError, WsError};
use super::transport::WsTransport;

thread_local! {
    static CFG: SharedConfig = SharedConfig::new("WS-CLIENT");
}

/// `WebSocket` client builder
pub struct WsClient<F, T> {
    connector: Pipeline<T>,
    head: Rc<RequestHead>,
    addr: Option<net::SocketAddr>,
    max_size: usize,
    server_mode: bool,
    timeout: Millis,
    extra_headers: RefCell<Option<HeaderMap>>,
    client_cfg: Rc<client::ClientConfig>,
    _t: marker::PhantomData<F>,
}

/// `WebSocket` client builder
pub struct WsClientBuilder<F, T> {
    inner: Option<Inner<F, T>>,
    err: Option<HttpError>,
    protocols: Option<String>,
    origin: Option<HeaderValue>,
    #[cfg(feature = "cookie")]
    cookies: Option<CookieJar>,
}

struct Inner<F, T> {
    connector: T,
    pub(crate) head: RequestHead,
    addr: Option<net::SocketAddr>,
    max_size: usize,
    server_mode: bool,
    timeout: Millis,
    _t: marker::PhantomData<F>,
}

impl WsClient<Base, ()> {
    /// Create new websocket client builder
    pub fn build<U>(uri: U) -> WsClientBuilder<Base, Connector<Uri>>
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        WsClientBuilder::new(uri)
    }

    /// Create new websocket client builder
    pub fn with_connector<F, T, U>(uri: U, connector: T) -> WsClientBuilder<F, T>
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
        F: Filter,
        T: ServiceFactory<
                Connect<Uri>,
                SharedConfig,
                Response = Io<F>,
                Error = ConnectError,
                InitError = (),
            >,
    {
        WsClientBuilder::new(uri).connector(connector)
    }
}

impl<F, T> WsClient<F, T> {
    /// Insert a header, replaces existing header.
    pub fn set_header<K, V>(&self, key: K, value: V) -> Result<(), HttpError>
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        let key = HeaderName::try_from(key).map_err(Into::into)?;
        let value = HeaderValue::try_from(value).map_err(Into::into)?;
        if let Some(headers) = &mut *self.extra_headers.borrow_mut() {
            headers.insert(key, value);
            return Ok(());
        }
        let mut headers = HeaderMap::new();
        headers.insert(key, value);
        *self.extra_headers.borrow_mut() = Some(headers);
        Ok(())
    }

    /// Set HTTP basic authorization header
    pub fn set_basic_auth<U>(
        &self,
        username: U,
        password: Option<&str>,
    ) -> Result<(), HttpError>
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.set_header(AUTHORIZATION, format!("Basic {}", base64.encode(auth)))
    }

    /// Set HTTP bearer authentication header
    pub fn set_bearer_auth<U>(&self, token: U) -> Result<(), HttpError>
    where
        U: fmt::Display,
    {
        self.set_header(AUTHORIZATION, format!("Bearer {token}"))
    }
}

impl<F, T> WsClient<F, T>
where
    F: Filter,
    T: Service<Connect<Uri>, Response = Io<F>, Error = ConnectError>,
{
    /// Complete request construction and connect to a websockets server.
    pub async fn connect(&self) -> Result<WsConnection<F>, WsClientError> {
        let head = self.head.clone();
        let max_size = self.max_size;
        let server_mode = self.server_mode;
        let to = self.timeout;
        let mut headers = self.extra_headers.borrow_mut().take().unwrap_or_default();

        // Generate a random key for the `Sec-WebSocket-Key` header.
        // a base64-encoded (see Section 4 of [RFC4648]) value that,
        // when decoded, is 16 bytes in length (RFC 6455)
        let mut sec_key: [u8; 16] = [0; 16];
        WyRand::new().fill(&mut sec_key);
        let key = base64.encode(sec_key);

        headers.insert(
            header::SEC_WEBSOCKET_KEY,
            HeaderValue::try_from(key.as_str()).unwrap(),
        );

        let msg = Connect::new(head.uri.clone()).set_addr(self.addr);
        log::trace!("Open ws connection to {:?} addr: {:?}", head.uri, self.addr);

        let io = self.connector.call(msg).await?;
        let tag = io.tag();

        // create Framed and send request
        let codec = h1::ClientCodec::default();

        // send request and read response
        let fut = async {
            log::trace!("{tag}: Sending ws handshake http message");
            io.send(
                (RequestHeadType::Rc(head, Some(headers)), BodySize::None).into(),
                &codec,
            )
            .await?;
            log::trace!("{tag}: Waiting for ws handshake response");
            io.recv(&codec)
                .await?
                .ok_or(WsClientError::Disconnected(None))
        };

        // set request timeout
        let response = if to.non_zero() {
            timeout(to, fut)
                .await
                .map_err(|_| WsClientError::Timeout)
                .and_then(|res| res)?
        } else {
            fut.await?
        };
        log::trace!("{tag}: Ws handshake response is received {response:?}");

        // verify response
        if response.status != StatusCode::SWITCHING_PROTOCOLS {
            return Err(WsClientError::InvalidResponseStatus(response.status));
        }

        // Check for "UPGRADE" to websocket header
        let has_hdr = if let Some(hdr) = response.headers.get(&header::UPGRADE) {
            if let Ok(s) = hdr.to_str() {
                s.to_ascii_lowercase().contains("websocket")
            } else {
                false
            }
        } else {
            false
        };
        if !has_hdr {
            log::trace!("{tag}: Invalid upgrade header");
            return Err(WsClientError::InvalidUpgradeHeader);
        }

        // Check for "CONNECTION" header
        if let Some(conn) = response.headers.get(&header::CONNECTION) {
            if let Ok(s) = conn.to_str() {
                if !s.to_ascii_lowercase().contains("upgrade") {
                    log::trace!("{tag}: Invalid connection header: {s}");
                    return Err(WsClientError::InvalidConnectionHeader(conn.clone()));
                }
            } else {
                log::trace!("{tag}: Invalid connection header: {conn:?}");
                return Err(WsClientError::InvalidConnectionHeader(conn.clone()));
            }
        } else {
            log::trace!("{tag}: Missing connection header");
            return Err(WsClientError::MissingConnectionHeader);
        }

        if let Some(hdr_key) = response.headers.get(&header::SEC_WEBSOCKET_ACCEPT) {
            let encoded = ws::hash_key(key.as_ref());
            if hdr_key.as_bytes() != encoded.as_bytes() {
                log::trace!(
                    "{tag}: Invalid challenge response: expected: {encoded} received: {key:?}"
                );
                return Err(WsClientError::InvalidChallengeResponse(
                    encoded,
                    hdr_key.clone(),
                ));
            }
        } else {
            log::trace!("{tag}: Missing SEC-WEBSOCKET-ACCEPT header");
            return Err(WsClientError::MissingWebSocketAcceptHeader);
        };
        log::trace!("{tag}: Ws handshake response verification is completed");

        // response and ws io
        Ok(WsConnection::new(
            io,
            ClientResponse::with_empty_payload(response, self.client_cfg.clone()),
            if server_mode {
                ws::Codec::new().max_size(max_size)
            } else {
                ws::Codec::new().max_size(max_size).client_mode()
            },
        ))
    }
}

impl<F, T> fmt::Debug for WsClient<F, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "\nWsClient {}:{}", self.head.method, self.head.uri)?;
        writeln!(f, "  headers:")?;
        for (key, val) in self.head.headers.iter() {
            writeln!(f, "    {key:?}: {val:?}")?;
        }
        Ok(())
    }
}

impl WsClientBuilder<Base, ()> {
    /// Create new websocket connector
    fn new<U>(uri: U) -> WsClientBuilder<Base, Connector<Uri>>
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        let (head, err) = match Uri::try_from(uri) {
            Ok(uri) => (
                RequestHead {
                    uri,
                    ..Default::default()
                },
                None,
            ),
            Err(e) => (Default::default(), Some(e.into())),
        };

        WsClientBuilder {
            err,
            origin: None,
            protocols: None,
            inner: Some(Inner {
                head,
                addr: None,
                max_size: 65_536,
                server_mode: false,
                timeout: Millis(5_000),
                connector: Connector::<Uri>::default(),
                _t: marker::PhantomData,
            }),
            #[cfg(feature = "cookie")]
            cookies: None,
        }
    }
}

impl<F, T> WsClientBuilder<F, T>
where
    T: ServiceFactory<
            Connect<Uri>,
            SharedConfig,
            Response = Io<F>,
            Error = ConnectError,
            InitError = (),
        >,
{
    /// Set socket address of the server.
    ///
    /// This address is used for connection. If address is not
    /// provided url's host name get resolved.
    pub fn address(&mut self, addr: net::SocketAddr) -> &mut Self {
        if let Some(parts) = parts(&mut self.inner, &self.err) {
            parts.addr = Some(addr);
        }
        self
    }

    /// Set supported websocket protocols
    pub fn protocols<U, V>(&mut self, protos: U) -> &mut Self
    where
        U: IntoIterator<Item = V>,
        V: AsRef<str>,
    {
        let mut protos = protos
            .into_iter()
            .fold(String::new(), |acc, s| acc + s.as_ref() + ",");
        protos.pop();
        self.protocols = Some(protos);
        self
    }

    #[cfg(feature = "cookie")]
    /// Set a cookie
    pub fn cookie<C>(&mut self, cookie: C) -> &mut Self
    where
        C: Into<Cookie<'static>>,
    {
        if self.cookies.is_none() {
            let mut jar = CookieJar::new();
            jar.add(cookie.into());
            self.cookies = Some(jar)
        } else {
            self.cookies.as_mut().unwrap().add(cookie.into());
        }
        self
    }

    /// Set request Origin
    pub fn origin<V, E>(&mut self, origin: V) -> &mut Self
    where
        HeaderValue: TryFrom<V, Error = E>,
        HttpError: From<E>,
    {
        match HeaderValue::try_from(origin) {
            Ok(value) => self.origin = Some(value),
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Set max frame size
    ///
    /// By default max size is set to 64kb
    pub fn max_frame_size(&mut self, size: usize) -> &mut Self {
        if let Some(parts) = parts(&mut self.inner, &self.err) {
            parts.max_size = size;
        }
        self
    }

    /// Disable payload masking. By default ws client masks frame payload.
    pub fn server_mode(&mut self) -> &mut Self {
        if let Some(parts) = parts(&mut self.inner, &self.err) {
            parts.server_mode = true;
        }
        self
    }

    /// Append a header.
    ///
    /// Header gets appended to existing header.
    /// To override header use `set_header()` method.
    pub fn header<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if let Some(parts) = parts(&mut self.inner, &self.err) {
            match HeaderName::try_from(key) {
                Ok(key) => match HeaderValue::try_from(value) {
                    Ok(value) => {
                        parts.head.headers.append(key, value);
                    }
                    Err(e) => self.err = Some(e.into()),
                },
                Err(e) => self.err = Some(e.into()),
            }
        }
        self
    }

    /// Insert a header, replaces existing header.
    pub fn set_header<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if let Some(parts) = parts(&mut self.inner, &self.err) {
            match HeaderName::try_from(key) {
                Ok(key) => match HeaderValue::try_from(value) {
                    Ok(value) => {
                        parts.head.headers.insert(key, value);
                    }
                    Err(e) => self.err = Some(e.into()),
                },
                Err(e) => self.err = Some(e.into()),
            }
        }
        self
    }

    /// Insert a header only if it is not yet set.
    pub fn set_header_if_none<K, V>(&mut self, key: K, value: V) -> &mut Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        if let Some(parts) = parts(&mut self.inner, &self.err) {
            match HeaderName::try_from(key) {
                Ok(key) => {
                    if !parts.head.headers.contains_key(&key) {
                        match HeaderValue::try_from(value) {
                            Ok(value) => {
                                parts.head.headers.insert(key, value);
                            }
                            Err(e) => self.err = Some(e.into()),
                        }
                    }
                }
                Err(e) => self.err = Some(e.into()),
            }
        }
        self
    }

    /// Set HTTP basic authorization header
    pub fn basic_auth<U>(&mut self, username: U, password: Option<&str>) -> &mut Self
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.header(AUTHORIZATION, format!("Basic {}", base64.encode(auth)))
    }

    /// Set HTTP bearer authentication header
    pub fn bearer_auth<U>(&mut self, token: U) -> &mut Self
    where
        U: fmt::Display,
    {
        self.header(AUTHORIZATION, format!("Bearer {token}"))
    }

    /// Set request timeout.
    ///
    /// Request timeout is the total time before a response must be received.
    /// Default value is 5 seconds.
    pub fn timeout<U: Into<Millis>>(&mut self, timeout: U) -> &mut Self {
        if let Some(parts) = parts(&mut self.inner, &self.err) {
            parts.timeout = timeout.into();
        }
        self
    }

    /// Use custom connector
    pub fn connector<F1, T1>(&mut self, connector: T1) -> WsClientBuilder<F1, T1>
    where
        F1: Filter,
        T1: ServiceFactory<
                Connect<Uri>,
                SharedConfig,
                Response = Io<F1>,
                Error = ConnectError,
                InitError = (),
            >,
    {
        let inner = self.inner.take().expect("cannot reuse WsClient builder");

        WsClientBuilder {
            inner: Some(Inner {
                connector,
                head: inner.head,
                addr: inner.addr,
                max_size: inner.max_size,
                server_mode: inner.server_mode,
                timeout: inner.timeout,
                _t: marker::PhantomData,
            }),
            err: self.err.take(),
            protocols: self.protocols.take(),
            origin: self.origin.take(),
            #[cfg(feature = "cookie")]
            cookies: self.cookies.take(),
        }
    }

    #[cfg(feature = "openssl")]
    /// Use openssl connector.
    pub fn openssl(
        &mut self,
        connector: tls_openssl::ssl::SslConnector,
    ) -> WsClientBuilder<Layer<openssl::SslFilter>, openssl::SslConnector<Connector<Uri>>>
    {
        self.connector(openssl::SslConnector::new(connector))
    }

    #[cfg(feature = "rustls")]
    /// Use rustls connector.
    pub fn rustls(
        &mut self,
        config: std::sync::Arc<tls_rustls::ClientConfig>,
    ) -> WsClientBuilder<Layer<rustls::TlsClientFilter>, rustls::TlsConnector<Connector<Uri>>>
    {
        self.connector(rustls::TlsConnector::from(config))
    }

    /// This method construct new `WsClientBuilder`
    pub fn take(&mut self) -> WsClientBuilder<F, T> {
        WsClientBuilder {
            inner: self.inner.take(),
            err: self.err.take(),
            origin: self.origin.take(),
            protocols: self.protocols.take(),
            #[cfg(feature = "cookie")]
            cookies: self.cookies.take(),
        }
    }

    /// Complete building process and construct websockets client.
    pub async fn finish(
        &mut self,
        cfg: SharedConfig,
    ) -> Result<WsClient<F, T::Service>, WsClientBuilderError> {
        if let Some(e) = self.err.take() {
            return Err(WsClientBuilderError::Http(e));
        }

        let mut inner = self.inner.take().expect("cannot reuse WsClient builder");

        // validate uri
        let uri = &inner.head.uri;
        if uri.host().is_none() {
            return Err(WsClientBuilderError::MissingHost);
        } else if uri.scheme().is_none() {
            return Err(WsClientBuilderError::MissingScheme);
        } else if let Some(scheme) = uri.scheme() {
            match scheme.as_str() {
                "http" | "ws" | "https" | "wss" => (),
                _ => return Err(WsClientBuilderError::UnknownScheme),
            }
        } else {
            return Err(WsClientBuilderError::UnknownScheme);
        }

        if !inner.head.headers.contains_key(header::HOST) {
            inner.head.headers.insert(
                header::HOST,
                HeaderValue::from_str(uri.host().unwrap()).unwrap(),
            );
        }

        #[cfg(feature = "cookie")]
        {
            use percent_encoding::percent_encode;
            use std::fmt::Write as FmtWrite;

            // set cookies
            if let Some(ref mut jar) = self.cookies {
                let mut cookie = String::new();
                for c in jar.delta() {
                    let name =
                        percent_encode(c.name().as_bytes(), crate::http::helpers::USERINFO);
                    let value = percent_encode(
                        c.value().as_bytes(),
                        crate::http::helpers::USERINFO,
                    );
                    let _ = write!(cookie, "; {name}={value}");
                }
                inner.head.headers.insert(
                    header::COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
            }
        }

        // origin
        if let Some(origin) = self.origin.take() {
            inner.head.headers.insert(header::ORIGIN, origin);
        }

        inner.head.set_connection_type(ConnectionType::Upgrade);
        inner
            .head
            .headers
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        inner.head.headers.insert(
            header::SEC_WEBSOCKET_VERSION,
            HeaderValue::from_static("13"),
        );

        if let Some(protocols) = self.protocols.take() {
            inner.head.headers.insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::try_from(protocols.as_str()).unwrap(),
            );
        }

        let connector = inner
            .connector
            .create(cfg)
            .await
            .map_err(|_| WsClientBuilderError::CannotCreateConnector)?;

        Ok(WsClient {
            connector: connector.into(),
            head: Rc::new(inner.head),
            addr: inner.addr,
            max_size: inner.max_size,
            server_mode: inner.server_mode,
            timeout: inner.timeout,
            extra_headers: RefCell::new(None),
            client_cfg: Default::default(),
            _t: marker::PhantomData,
        })
    }
}

#[inline]
fn parts<'a, F, T>(
    parts: &'a mut Option<Inner<F, T>>,
    err: &Option<HttpError>,
) -> Option<&'a mut Inner<F, T>> {
    if err.is_some() {
        return None;
    }
    parts.as_mut()
}

impl<F, T> fmt::Debug for WsClientBuilder<F, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref parts) = self.inner {
            writeln!(
                f,
                "\nWsClientBuilder {}:{}",
                parts.head.method, parts.head.uri
            )?;
            writeln!(f, "  headers:")?;
            for (key, val) in parts.head.headers.iter() {
                writeln!(f, "    {key:?}: {val:?}")?;
            }
        } else {
            writeln!(f, "WsClientBuilder(Consumed)")?;
        }
        Ok(())
    }
}

pub struct WsConnection<F> {
    io: Io<F>,
    codec: ws::Codec,
    res: ClientResponse,
}

impl<F> WsConnection<F> {
    fn new(io: Io<F>, res: ClientResponse, codec: ws::Codec) -> Self {
        Self { io, codec, res }
    }

    /// Get codec reference
    pub fn codec(&self) -> &ws::Codec {
        &self.codec
    }

    /// Get reference to response
    pub fn response(&self) -> &ClientResponse {
        &self.res
    }
}

impl<F> WsConnection<F> {
    /// Get ws sink
    pub fn sink(&self) -> ws::WsSink {
        ws::WsSink::new(self.io.get_ref(), self.codec.clone())
    }

    /// Consumes the `WsConnection`, returning it'as underlying I/O stream object
    /// and response.
    pub fn into_inner(self) -> (Io<F>, ws::Codec, ClientResponse) {
        (self.io, self.codec, self.res)
    }
}

impl WsConnection<Sealed> {
    // TODO: fix close frame handling
    /// Start client websockets with `SinkService` and `mpsc::Receiver<Frame>`
    pub fn receiver(self) -> mpsc::Receiver<Result<ws::Frame, WsError<()>>> {
        let (tx, rx): (_, mpsc::Receiver<Result<ws::Frame, WsError<()>>>) = mpsc::channel();

        let _ = rt::spawn(async move {
            let tx2 = tx.clone();
            let io = self.io.get_ref();

            let result = self
                .start(fn_service(move |item: ws::Frame| {
                    match tx.send(Ok(item)) {
                        Ok(()) => (),
                        Err(_) => io.close(),
                    };
                    Ready::Ok::<Option<ws::Message>, ()>(None)
                }))
                .await;

            if let Err(e) = result {
                let _ = tx2.send(Err(e));
            }
        });

        rx
    }

    /// Start client websockets service.
    pub async fn start<T, U>(self, service: U) -> Result<(), WsError<T::Error>>
    where
        T: Service<ws::Frame, Response = Option<ws::Message>> + 'static,
        U: IntoService<T, ws::Frame>,
    {
        let service = apply_fn(
            service.into_service().map_err(WsError::Service),
            |req, svc| async move {
                match req {
                    DispatchItem::<ws::Codec>::Item(item) => svc.call(item).await,
                    DispatchItem::WBackPressureEnabled
                    | DispatchItem::WBackPressureDisabled => Ok(None),
                    DispatchItem::KeepAliveTimeout => Err(WsError::KeepAlive),
                    DispatchItem::ReadTimeout => Err(WsError::ReadTimeout),
                    DispatchItem::DecoderError(e) | DispatchItem::EncoderError(e) => {
                        Err(WsError::Protocol(e))
                    }
                    DispatchItem::Disconnect(e) => Err(WsError::Disconnected(e)),
                }
            },
        );

        Dispatcher::new(self.io, self.codec, service).await
    }
}

impl<F: Filter> WsConnection<F> {
    /// Convert I/O stream to boxed stream
    pub fn seal(self) -> WsConnection<Sealed> {
        WsConnection {
            io: self.io.seal(),
            codec: self.codec,
            res: self.res,
        }
    }

    /// Convert to ws stream to plain io stream
    pub fn into_transport(self) -> Io<Layer<WsTransport, F>> {
        WsTransport::create(self.io, self.codec)
    }
}

impl<F> fmt::Debug for WsConnection<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsConnection")
            .field("response", &self.res)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::rt_test]
    async fn test_debug() {
        let mut builder = WsClient::build("http://localhost")
            .header("x-test", "111")
            .take();
        let repr = format!("{builder:?}");
        assert!(repr.contains("WsClientBuilder"));
        assert!(repr.contains("x-test"));

        let client = builder.finish(SharedConfig::default()).await.unwrap();
        let repr = format!("{client:?}");
        assert!(repr.contains("WsClient"));
        assert!(repr.contains("x-test"));
    }

    #[crate::rt_test]
    async fn header_override() {
        let req = WsClient::build("http://localhost")
            .header(header::CONTENT_TYPE, "111")
            .set_header(header::CONTENT_TYPE, "222")
            .finish(SharedConfig::default())
            .await
            .unwrap();

        assert_eq!(
            req.head
                .headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "222"
        );
    }

    #[crate::rt_test]
    async fn basic_errs() {
        let err = WsClient::build("localhost")
            .finish(SharedConfig::default())
            .await
            .err()
            .unwrap();
        assert!(matches!(err, WsClientBuilderError::MissingScheme));
        let err = WsClient::build("unknown://localhost")
            .finish(SharedConfig::default())
            .await
            .err()
            .unwrap();
        assert!(matches!(err, WsClientBuilderError::UnknownScheme));
        let err = WsClient::build("/")
            .finish(SharedConfig::default())
            .await
            .err()
            .unwrap();
        assert!(matches!(err, WsClientBuilderError::MissingHost));
    }

    #[crate::rt_test]
    async fn basic_auth() {
        let client = WsClient::build("http://localhost")
            .basic_auth("username", Some("password"))
            .finish(SharedConfig::default())
            .await
            .unwrap();
        assert_eq!(
            client
                .head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );

        let client = WsClient::build("http://localhost")
            .basic_auth("username", None)
            .finish(SharedConfig::default())
            .await
            .unwrap();
        assert_eq!(
            client
                .head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6"
        );

        client.set_basic_auth("username", Some("password")).unwrap();
        assert_eq!(
            client
                .extra_headers
                .borrow()
                .as_ref()
                .unwrap()
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );
    }

    #[crate::rt_test]
    #[allow(clippy::let_underscore_future)]
    async fn bearer_auth() {
        let client = WsClient::build("http://localhost")
            .bearer_auth("someS3cr3tAutht0k3n")
            .finish(SharedConfig::default())
            .await
            .unwrap();
        assert_eq!(
            client
                .head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k3n"
        );

        let _ = client.set_bearer_auth("someS3cr3tAutht0k2n");
        assert_eq!(
            client
                .extra_headers
                .borrow()
                .as_ref()
                .unwrap()
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k2n"
        );

        let _ = client.connect();
    }

    #[cfg(feature = "cookie")]
    #[crate::rt_test]
    async fn basics() {
        let mut builder = WsClient::build("http://localhost/")
            .origin("test-origin")
            .max_frame_size(100)
            .server_mode()
            .protocols(["v1", "v2"])
            .set_header_if_none(header::CONTENT_TYPE, "json")
            .set_header_if_none(header::CONTENT_TYPE, "text")
            .cookie(Cookie::build(("cookie1", "value1")))
            .take();
        assert_eq!(
            builder.origin.as_ref().unwrap().to_str().unwrap(),
            "test-origin"
        );
        assert_eq!(builder.inner.as_ref().unwrap().max_size, 100);
        assert!(builder.inner.as_ref().unwrap().server_mode);
        assert_eq!(builder.protocols, Some("v1,v2".to_string()));

        let client = builder.finish(SharedConfig::default()).await.unwrap();
        assert_eq!(
            client.head.headers.get(header::CONTENT_TYPE).unwrap(),
            header::HeaderValue::from_static("json")
        );

        let _ = client.connect().await;

        assert!(
            WsClient::build("/")
                .finish(SharedConfig::default())
                .await
                .is_err()
        );
        assert!(
            WsClient::build("http:///test")
                .finish(SharedConfig::default())
                .await
                .is_err()
        );
        assert!(
            WsClient::build("hmm://test.com/")
                .finish(SharedConfig::default())
                .await
                .is_err()
        );
    }
}
