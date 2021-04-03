//! Websockets client
use std::{convert::TryFrom, fmt, net::SocketAddr, rc::Rc, str};

#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};
use nanorand::{WyRand, RNG};

use crate::codec::{AsyncRead, AsyncWrite, Framed};
use crate::framed::{DispatchItem, Dispatcher, State};
use crate::http::error::HttpError;
use crate::http::header::{self, HeaderName, HeaderValue, AUTHORIZATION};
use crate::http::{ConnectionType, Payload, RequestHead, StatusCode, Uri};
use crate::service::{apply_fn, into_service, IntoService, Service};
use crate::util::Either;
use crate::{channel::mpsc, rt, rt::time::timeout, util::sink, util::Ready, ws};

pub use crate::ws::{CloseCode, CloseReason, Frame, Message};

use super::connect::BoxedSocket;
use super::error::{InvalidUrl, SendRequestError, WsClientError};
use super::response::ClientResponse;
use super::ClientConfig;

/// `WebSocket` connection
pub struct WsRequest {
    pub(crate) head: RequestHead,
    err: Option<HttpError>,
    origin: Option<HeaderValue>,
    protocols: Option<String>,
    addr: Option<SocketAddr>,
    max_size: usize,
    server_mode: bool,
    #[cfg(feature = "cookie")]
    cookies: Option<CookieJar>,
    config: Rc<ClientConfig>,
}

impl WsRequest {
    /// Create new websocket connection
    pub(super) fn new<U>(uri: U, config: Rc<ClientConfig>) -> Self
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

        WsRequest {
            head,
            err,
            config,
            addr: None,
            origin: None,
            protocols: None,
            max_size: 65_536,
            server_mode: false,
            #[cfg(feature = "cookie")]
            cookies: None,
        }
    }

    /// Set socket address of the server.
    ///
    /// This address is used for connection. If address is not
    /// provided url's host name get resolved.
    pub fn address(mut self, addr: SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Set supported websocket protocols
    pub fn protocols<U, V>(mut self, protos: U) -> Self
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
    pub fn cookie(mut self, cookie: Cookie<'_>) -> Self {
        if self.cookies.is_none() {
            let mut jar = CookieJar::new();
            jar.add(cookie.into_owned());
            self.cookies = Some(jar)
        } else {
            self.cookies.as_mut().unwrap().add(cookie.into_owned());
        }
        self
    }

    /// Set request Origin
    pub fn origin<V, E>(mut self, origin: V) -> Self
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
    pub fn max_frame_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    /// Disable payload masking. By default ws client masks frame payload.
    pub fn server_mode(mut self) -> Self {
        self.server_mode = true;
        self
    }

    /// Append a header.
    ///
    /// Header gets appended to existing header.
    /// To override header use `set_header()` method.
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => {
                    self.head.headers.append(key, value);
                }
                Err(e) => self.err = Some(e.into()),
            },
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Insert a header, replaces existing header.
    pub fn set_header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => {
                    self.head.headers.insert(key, value);
                }
                Err(e) => self.err = Some(e.into()),
            },
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Insert a header only if it is not yet set.
    pub fn set_header_if_none<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => {
                if !self.head.headers.contains_key(&key) {
                    match HeaderValue::try_from(value) {
                        Ok(value) => {
                            self.head.headers.insert(key, value);
                        }
                        Err(e) => self.err = Some(e.into()),
                    }
                }
            }
            Err(e) => self.err = Some(e.into()),
        }
        self
    }

    /// Set HTTP basic authorization header
    pub fn basic_auth<U>(self, username: U, password: Option<&str>) -> Self
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{}:{}", username, password),
            None => format!("{}:", username),
        };
        self.header(AUTHORIZATION, format!("Basic {}", base64::encode(&auth)))
    }

    /// Set HTTP bearer authentication header
    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: fmt::Display,
    {
        self.header(AUTHORIZATION, format!("Bearer {}", token))
    }

    /// Complete request construction and connect to a websockets server.
    pub async fn connect(mut self) -> Result<WsConnection, WsClientError> {
        if let Some(e) = self.err.take() {
            return Err(WsClientError::from(e));
        }

        // validate uri
        let uri = &self.head.uri;
        if uri.host().is_none() {
            return Err(InvalidUrl::MissingHost.into());
        } else if uri.scheme().is_none() {
            return Err(InvalidUrl::MissingScheme.into());
        } else if let Some(scheme) = uri.scheme() {
            match scheme.as_str() {
                "http" | "ws" | "https" | "wss" => (),
                _ => return Err(InvalidUrl::UnknownScheme.into()),
            }
        } else {
            return Err(InvalidUrl::UnknownScheme.into());
        }

        if !self.head.headers.contains_key(header::HOST) {
            self.head.headers.insert(
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
                    let name = percent_encode(
                        c.name().as_bytes(),
                        crate::http::helpers::USERINFO,
                    );
                    let value = percent_encode(
                        c.value().as_bytes(),
                        crate::http::helpers::USERINFO,
                    );
                    let _ = write!(&mut cookie, "; {}={}", name, value);
                }
                self.head.headers.insert(
                    header::COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
            }
        }

        // origin
        if let Some(origin) = self.origin.take() {
            self.head.headers.insert(header::ORIGIN, origin);
        }

        self.head.set_connection_type(ConnectionType::Upgrade);
        self.head
            .headers
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        self.head.headers.insert(
            header::SEC_WEBSOCKET_VERSION,
            HeaderValue::from_static("13"),
        );

        if let Some(protocols) = self.protocols.take() {
            self.head.headers.insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::try_from(protocols.as_str()).unwrap(),
            );
        }

        // Generate a random key for the `Sec-WebSocket-Key` header.
        // a base64-encoded (see Section 4 of [RFC4648]) value that,
        // when decoded, is 16 bytes in length (RFC 6455)
        let mut sec_key: [u8; 16] = [0; 16];
        WyRand::new().fill(&mut sec_key);
        let key = base64::encode(&sec_key);

        self.head.headers.insert(
            header::SEC_WEBSOCKET_KEY,
            HeaderValue::try_from(key.as_str()).unwrap(),
        );

        let head = self.head;
        let max_size = self.max_size;
        let server_mode = self.server_mode;

        let fut = self.config.connector.open_tunnel(head.into(), self.addr);

        // set request timeout
        let (head, framed) = if let Some(to) = self.config.timeout {
            timeout(to, fut)
                .await
                .map_err(|_| SendRequestError::Timeout)
                .and_then(|res| res)?
        } else {
            fut.await?
        };

        // verify response
        if head.status != StatusCode::SWITCHING_PROTOCOLS {
            return Err(WsClientError::InvalidResponseStatus(head.status));
        }

        // Check for "UPGRADE" to websocket header
        let has_hdr = if let Some(hdr) = head.headers.get(&header::UPGRADE) {
            if let Ok(s) = hdr.to_str() {
                s.to_ascii_lowercase().contains("websocket")
            } else {
                false
            }
        } else {
            false
        };
        if !has_hdr {
            log::trace!("Invalid upgrade header");
            return Err(WsClientError::InvalidUpgradeHeader);
        }

        // Check for "CONNECTION" header
        if let Some(conn) = head.headers.get(&header::CONNECTION) {
            if let Ok(s) = conn.to_str() {
                if !s.to_ascii_lowercase().contains("upgrade") {
                    log::trace!("Invalid connection header: {}", s);
                    return Err(WsClientError::InvalidConnectionHeader(conn.clone()));
                }
            } else {
                log::trace!("Invalid connection header: {:?}", conn);
                return Err(WsClientError::InvalidConnectionHeader(conn.clone()));
            }
        } else {
            log::trace!("Missing connection header");
            return Err(WsClientError::MissingConnectionHeader);
        }

        if let Some(hdr_key) = head.headers.get(&header::SEC_WEBSOCKET_ACCEPT) {
            let encoded = ws::hash_key(key.as_ref());
            if hdr_key.as_bytes() != encoded.as_bytes() {
                log::trace!(
                    "Invalid challenge response: expected: {} received: {:?}",
                    encoded,
                    key
                );
                return Err(WsClientError::InvalidChallengeResponse(
                    encoded,
                    hdr_key.clone(),
                ));
            }
        } else {
            log::trace!("Missing SEC-WEBSOCKET-ACCEPT header");
            return Err(WsClientError::MissingWebSocketAcceptHeader);
        };

        // response and ws io
        Ok(WsConnection::new(
            ClientResponse::new(head, Payload::None),
            framed.map_codec(|_| {
                if server_mode {
                    ws::Codec::new().max_size(max_size)
                } else {
                    ws::Codec::new().max_size(max_size).client_mode()
                }
            }),
        ))
    }
}

impl fmt::Debug for WsRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "\nWebsocketsRequest {}:{}",
            self.head.method, self.head.uri
        )?;
        writeln!(f, "  headers:")?;
        for (key, val) in self.head.headers.iter() {
            writeln!(f, "    {:?}: {:?}", key, val)?;
        }
        Ok(())
    }
}

pub struct WsConnection<Io = BoxedSocket> {
    io: Io,
    state: State,
    codec: ws::Codec,
    res: ClientResponse,
}

impl<Io> WsConnection<Io>
where
    Io: AsyncRead + AsyncWrite + Unpin + 'static,
{
    fn new(res: ClientResponse, framed: Framed<Io, ws::Codec>) -> Self {
        let (io, codec, state) = State::from_framed(framed);

        Self {
            io,
            codec,
            state,
            res,
        }
    }

    /// Get ws sink
    pub fn sink(&self) -> ws::WsSink {
        ws::WsSink::new(self.state.clone(), self.codec.clone())
    }

    /// Get reference to response
    pub fn response(&self) -> &ClientResponse {
        &self.res
    }

    /// Start client websockets with `SinkService` and `mpsc::Receiver<Frame>`
    pub fn start_default(self) -> mpsc::Receiver<Result<ws::Frame, ws::WsError<()>>> {
        let (tx, rx): (_, mpsc::Receiver<Result<ws::Frame, ws::WsError<()>>>) =
            mpsc::channel();

        rt::spawn(async move {
            let srv = sink::SinkService::new(tx.clone()).map(|_| None);

            if let Err(err) = self
                .start(into_service(move |item| {
                    let fut = srv.call(Ok::<_, ws::WsError<()>>(item));
                    async move { fut.await.map_err(|_| ()) }
                }))
                .await
            {
                let _ = tx.send(Err(err));
            }
        });

        rx
    }

    /// Start client websockets service.
    pub async fn start<T, F>(self, service: F) -> Result<(), ws::WsError<T::Error>>
    where
        T: Service<Request = ws::Frame, Response = Option<ws::Message>> + 'static,
        F: IntoService<T>,
    {
        let service = apply_fn(
            service.into_service().map_err(ws::WsError::Service),
            |req, srv| match req {
                DispatchItem::Item(item) => Either::Left(srv.call(item)),
                DispatchItem::WBackPressureEnabled
                | DispatchItem::WBackPressureDisabled => Either::Right(Ready::Ok(None)),
                DispatchItem::KeepAliveTimeout => {
                    Either::Right(Ready::Err(ws::WsError::KeepAlive))
                }
                DispatchItem::DecoderError(e) | DispatchItem::EncoderError(e) => {
                    Either::Right(Ready::Err(ws::WsError::Protocol(e)))
                }
                DispatchItem::IoError(e) => {
                    Either::Right(Ready::Err(ws::WsError::Io(e)))
                }
            },
        );

        Dispatcher::new(self.io, self.codec, self.state, service, Default::default())
            .await
    }

    /// Consumes the `WsConnection`, returning it'as underlying I/O framed object
    /// and response.
    pub fn into_inner(self) -> (ClientResponse, Framed<Io, ws::Codec>) {
        let framed = self.state.into_framed(self.io, self.codec);
        (self.res, framed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::client::Client;

    #[crate::rt_test]
    async fn test_debug() {
        let request = Client::new().ws("/").header("x-test", "111");
        let repr = format!("{:?}", request);
        assert!(repr.contains("WebsocketsRequest"));
        assert!(repr.contains("x-test"));
    }

    #[crate::rt_test]
    async fn test_header_override() {
        let req = Client::build()
            .header(header::CONTENT_TYPE, "111")
            .finish()
            .ws("/")
            .set_header(header::CONTENT_TYPE, "222");

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
    async fn basic_auth() {
        let req = Client::new()
            .ws("/")
            .basic_auth("username", Some("password"));
        assert_eq!(
            req.head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );

        let req = Client::new().ws("/").basic_auth("username", None);
        assert_eq!(
            req.head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6"
        );
    }

    #[crate::rt_test]
    async fn bearer_auth() {
        let req = Client::new().ws("/").bearer_auth("someS3cr3tAutht0k3n");
        assert_eq!(
            req.head
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k3n"
        );
        let _ = req.connect();
    }

    #[cfg(feature = "cookie")]
    #[crate::rt_test]
    async fn basics() {
        let req = Client::new()
            .ws("http://localhost/")
            .origin("test-origin")
            .max_frame_size(100)
            .server_mode()
            .protocols(&["v1", "v2"])
            .set_header_if_none(header::CONTENT_TYPE, "json")
            .set_header_if_none(header::CONTENT_TYPE, "text")
            .cookie(Cookie::build("cookie1", "value1").finish());
        assert_eq!(
            req.origin.as_ref().unwrap().to_str().unwrap(),
            "test-origin"
        );
        assert_eq!(req.max_size, 100);
        assert_eq!(req.server_mode, true);
        assert_eq!(req.protocols, Some("v1,v2".to_string()));
        assert_eq!(
            req.head.headers.get(header::CONTENT_TYPE).unwrap(),
            header::HeaderValue::from_static("json")
        );

        let _ = req.connect().await;

        assert!(Client::new().ws("/").connect().await.is_err());
        assert!(Client::new().ws("http:///test").connect().await.is_err());
        assert!(Client::new().ws("hmm://test.com/").connect().await.is_err());
    }
}
