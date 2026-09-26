//! WebSocket client.
use std::{fmt, marker};

#[cfg(feature = "openssl")]
use crate::connect::openssl;
#[cfg(feature = "openssl")]
use tls_openssl::ssl::SslConnector;

#[cfg(feature = "rustls")]
use crate::connect::rustls::{TlsClientFilter, TlsConnector};
#[cfg(feature = "rustls")]
use tls_rustls::ClientConfig as RustlsClientConfig;

use base64::{Engine, engine::general_purpose::STANDARD as base64};
use nanorand::Rng;

use crate::client::{ClientCodec, ClientConfig, ClientRawRequest, ClientResponse};
use crate::connect::{Connect, ConnectError, Connector};
use crate::error::{Error, ErrorMapping};
use crate::http::header::{self, HeaderMap, HeaderValue};
use crate::http::{ConnectionType, Message, Method, RequestHead, StatusCode, Uri};
use crate::http::{body::BodySize, error::HttpError};
use crate::io::{Base, DispatchItem, Dispatcher, Filter, Io, Layer, Reason, Sealed};
use crate::service::{IntoService, Pipeline, apply_fn, fn_service};
use crate::{Cfg, Service, SharedCfg, channel::mpsc, rt, time::timeout, ws};

use super::cfg::is_token;
use super::error::{WsClientError, WsConfigError, WsError};
use super::proto::{CloseCode, CloseReason};
use super::{WsClientConfig, handshake::header_contains_token, transport::WsTransport};

thread_local! {
    static CFG: SharedCfg = SharedCfg::new("WS-CLIENT").into();
}

/// Builder for establishing a WebSocket client connection.
///
/// The builder contains the target URI and a typed [`WsClientConfig`]. Use
/// [`connect`](Self::connect) to perform the opening handshake.
pub struct WsClient<F> {
    uri: Uri,
    err: Option<WsConfigError>,
    cfg: Cfg<WsClientConfig>,
    http_cfg: Cfg<ClientConfig>,
    connector: Pipeline<Connect<Uri>, Io<F>, Error<ConnectError>>,
    filter: marker::PhantomData<F>,
}

impl WsClient<Base> {
    /// Creates a client for `uri` using the supplied configuration.
    ///
    /// ```rust
    /// use ntex::{SharedCfg, time::Seconds};
    /// use ntex::ws::{WsClient, WsClientConfig};
    ///
    /// #[ntex::main]
    /// async fn main() {
    ///     let cfg = SharedCfg::new("WS-CLIENT").add(
    ///         WsClientConfig::new()
    ///             .set_max_frame_size(128 * 1024)
    ///             .set_handshake_timeout(Seconds(10))
    ///     );
    ///
    ///     let _client = WsClient::new("ws://localhost/socket", cfg);
    /// }
    /// ```
    ///
    /// URI conversion and validation errors are stored and returned by
    /// [`connect`](Self::connect).
    pub fn new<U>(uri: U, cfg: impl Into<Cfg<WsClientConfig>>) -> Self
    where
        Uri: TryFrom<U>,
        HttpError: From<<Uri as TryFrom<U>>::Error>,
    {
        let (uri, err) = match Uri::try_from(uri) {
            Ok(uri) => {
                let err = if uri.host().is_none() {
                    Some(WsConfigError::MissingHost)
                } else if uri.scheme().is_none() {
                    Some(WsConfigError::MissingScheme)
                } else if let Some(scheme) = uri.scheme() {
                    if matches!(scheme.as_str(), "http" | "ws" | "https" | "wss") {
                        None
                    } else {
                        Some(WsConfigError::UnknownScheme)
                    }
                } else {
                    Some(WsConfigError::UnknownScheme)
                };
                (uri, err)
            }
            Err(err) => (
                Uri::default(),
                Some(WsConfigError::Http(HttpError::from(err))),
            ),
        };

        let cfg = cfg.into();
        let shared = cfg.shared();

        WsClient {
            uri,
            err,
            cfg,
            http_cfg: shared.get(),
            connector: Pipeline::new(shared, Connector::<Uri>::new()),
            filter: marker::PhantomData,
        }
    }
}

impl<F> WsClient<F> {
    /// Replaces the network connector used to establish the connection.
    pub fn connector<U, S>(self, f: impl IntoService<S, SharedCfg, Connect<Uri>>) -> WsClient<U>
    where
        U: Filter + 'static,
        S: Service<SharedCfg, Connect<Uri>, Res = Io<U>, Error = Error<ConnectError>> + 'static,
    {
        let shared = self.cfg.shared();
        WsClient {
            uri: self.uri,
            err: self.err,
            cfg: self.cfg,
            http_cfg: self.http_cfg,
            connector: Pipeline::new(shared, f.into_service()),
            filter: marker::PhantomData,
        }
    }

    #[cfg(feature = "openssl")]
    /// Uses the supplied OpenSSL connector for secure connections.
    pub fn openssl(self, config: SslConnector) -> WsClient<Layer<openssl::SslFilter>> {
        self.connector(openssl::SslConnector::new(config))
    }

    #[cfg(feature = "rustls")]
    /// Uses the supplied rustls connector for secure connections.
    pub fn rustls(
        self,
        config: std::sync::Arc<RustlsClientConfig>,
    ) -> WsClient<Layer<TlsClientFilter>> {
        self.connector(TlsConnector::from(config))
    }
}

impl<F> WsClient<F>
where
    F: Filter,
{
    /// Establishes the connection and performs the WebSocket opening handshake.
    ///
    /// # Errors
    ///
    /// Returns an error if connection establishment, HTTP encoding or decoding,
    /// URI validation, timeout handling, or handshake validation fails.
    pub async fn connect(&self) -> Result<WsConnection<F>, Error<WsClientError>> {
        if let Some(err) = self.err.clone() {
            return Err(Error::from(WsClientError::Config(err)).set_service(self.cfg.service()));
        }

        let mut head = Message::<RequestHead>::new();
        // the message pool may return a recycled head whose method is not GET
        // (e.g. previously used by the HTTP/1 server dispatcher for a POST request)
        head.method = Method::GET;
        head.uri = self.uri.clone();
        head.set_connection_type(ConnectionType::Upgrade);

        // copy headers
        for (key, value) in &self.cfg.headers {
            if !head.headers().contains_key(key) {
                head.headers_mut().insert(key.clone(), value.clone());
            }
        }

        // host header
        if !head.headers.contains_key(header::HOST) {
            let val = HeaderValue::from_str(self.uri.authority().unwrap().as_str()).unwrap();
            head.headers.insert(header::HOST, val);
        }

        #[cfg(feature = "cookie")]
        {
            use percent_encoding::percent_encode;
            use std::fmt::Write as FmtWrite;

            // set cookies
            if let Some(ref jar) = self.cfg.cookies {
                let mut cookie = String::new();
                for c in jar.delta() {
                    let name = percent_encode(c.name().as_bytes(), crate::http::helpers::USERINFO);
                    let value =
                        percent_encode(c.value().as_bytes(), crate::http::helpers::USERINFO);
                    let _ = write!(cookie, "; {name}={value}");
                }
                head.headers.insert(
                    header::COOKIE,
                    HeaderValue::from_str(&cookie.as_str()[2..]).unwrap(),
                );
            }
        }

        // Generate a random key for the `Sec-WebSocket-Key` header.
        // a base64-encoded (see Section 4 of [RFC4648]) value that,
        // when decoded, is 16 bytes in length (RFC 6455)
        let mut sec_key: [u8; 16] = [0; 16];
        nanorand::tls_rng().fill(&mut sec_key);
        let key = base64.encode(sec_key);

        head.headers.insert(
            header::SEC_WEBSOCKET_KEY,
            HeaderValue::try_from(key.as_str()).unwrap(),
        );

        let msg = Connect::new(self.uri.clone()).set_addr(self.cfg.addr);
        log::trace!(
            "{}: Open ws connection to {:?} addr: {:?}",
            self.cfg.tag(),
            self.uri,
            self.cfg.addr
        );

        let io = self.connector.call(msg).await.into_error()?;
        let tag = io.tag();

        // create Framed and send request
        let codec = ClientCodec::new(true, io.shared().get());

        // send request and read response
        let fut = async {
            log::trace!("{tag}: Sending ws handshake http message");
            io.send(
                ClientRawRequest {
                    head,
                    headers: None,
                    size: BodySize::None,
                }
                .into(),
                &codec,
            )
            .await?;
            log::trace!("{tag}: Waiting for ws handshake response");
            io.recv(&codec)
                .await?
                .ok_or(WsClientError::Disconnected(None))
        };

        // set request timeout
        let response = if self.cfg.timeout.non_zero() {
            timeout(self.cfg.timeout, fut)
                .await
                .map_err(|()| WsClientError::Timeout)
                .and_then(|res| res)?
        } else {
            fut.await?
        };
        log::trace!("{tag}: Ws handshake response is received {response:?}");

        // verify response
        if response.status != StatusCode::SWITCHING_PROTOCOLS {
            return Err(Error::from(WsClientError::InvalidResponseStatus(
                response.status,
            )));
        }

        // Check for "UPGRADE" to websocket header
        if !header_contains_token(&response.headers, &header::UPGRADE, "websocket") {
            log::trace!("{tag}: Invalid upgrade header");
            return Err(Error::from(WsClientError::InvalidUpgradeHeader));
        }

        // Check for "CONNECTION" header
        if let Some(conn) = response.headers.get(&header::CONNECTION) {
            if !header_contains_token(&response.headers, &header::CONNECTION, "upgrade") {
                log::trace!("{tag}: Invalid connection header: {conn:?}");
                return Err(Error::from(WsClientError::InvalidConnectionHeader(
                    conn.clone(),
                )));
            }
        } else {
            log::trace!("{tag}: Missing connection header");
            return Err(Error::from(WsClientError::MissingConnectionHeader));
        }

        if let Some(hdr_key) = response.headers.get(&header::SEC_WEBSOCKET_ACCEPT) {
            let encoded = ws::hash_key(key.as_ref()).map_err(|_| {
                Error::from(WsClientError::InvalidChallengeResponse(
                    String::new(),
                    hdr_key.clone(),
                ))
            })?;
            if hdr_key.as_bytes() != encoded.as_bytes() {
                log::trace!(
                    "{tag}: Invalid challenge response: expected: {encoded} received: {key:?}"
                );
                return Err(Error::from(WsClientError::InvalidChallengeResponse(
                    encoded,
                    hdr_key.clone(),
                )));
            }
        } else {
            log::trace!("{tag}: Missing SEC-WEBSOCKET-ACCEPT header");
            return Err(Error::from(WsClientError::MissingWebSocketAcceptHeader));
        }

        validate_negotiation(&response.headers, &self.cfg.headers).map_err(Error::from)?;
        log::trace!("{tag}: Ws handshake response verification is completed");

        // response and ws io
        Ok(WsConnection::new(
            io,
            ClientResponse::with_empty_payload(response, self.http_cfg.clone()),
            if self.cfg.server_mode {
                ws::Codec::new().max_size(self.cfg.max_size)
            } else {
                ws::Codec::new()
                    .max_size(self.cfg.max_size)
                    .set_client_mode()
            },
        ))
    }
}

fn validate_negotiation(response: &HeaderMap, offered: &HeaderMap) -> Result<(), WsClientError> {
    if let Some(extensions) = response.get(header::SEC_WEBSOCKET_EXTENSIONS) {
        return Err(WsClientError::UnexpectedWebSocketExtensions(
            extensions.clone(),
        ));
    }

    let mut protocols = response.get_all(header::SEC_WEBSOCKET_PROTOCOL);
    if let Some(protocol) = protocols.next() {
        let selected = protocol.to_str().ok();
        let valid = protocols.next().is_none()
            && selected.is_some_and(|selected| {
                is_token(selected)
                    && offered
                        .get(header::SEC_WEBSOCKET_PROTOCOL)
                        .and_then(|offered| offered.to_str().ok())
                        .is_some_and(|offered| {
                            offered.split(',').any(|item| item.trim() == selected)
                        })
            });
        if !valid {
            return Err(WsClientError::InvalidWebSocketProtocol(protocol.clone()));
        }
    }
    Ok(())
}

impl<F> fmt::Debug for WsClient<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsClient").field("cfg", &self.cfg).finish()
    }
}

/// An established WebSocket client connection.
///
/// This value retains the opening-handshake response, the WebSocket codec, and
/// the underlying I/O stream.
pub struct WsConnection<F> {
    io: Io<F>,
    codec: ws::Codec,
    res: ClientResponse,
}

impl<F> WsConnection<F> {
    fn new(io: Io<F>, res: ClientResponse, codec: ws::Codec) -> Self {
        Self { io, codec, res }
    }

    /// Returns the connection's WebSocket codec.
    pub fn codec(&self) -> &ws::Codec {
        &self.codec
    }

    /// Returns the opening-handshake response.
    pub fn response(&self) -> &ClientResponse {
        &self.res
    }
}

impl<F> WsConnection<F> {
    /// Creates a sink for sending messages over this connection.
    pub fn sink(&self) -> ws::WsSink {
        ws::WsSink::new(
            self.io.get_ref(),
            self.codec.clone(),
            self.io.shared().get(),
        )
    }

    /// Consumes the connection and returns its I/O stream, codec, and
    /// opening-handshake response.
    pub fn into_inner(self) -> (Io<F>, ws::Codec, ClientResponse) {
        (self.io, self.codec, self.res)
    }
}

impl WsConnection<Sealed> {
    /// Starts the WebSocket dispatcher and returns a channel of received frames.
    ///
    /// The dispatcher runs in a spawned task. Protocol and connection errors
    /// are delivered through the returned channel.
    pub fn receiver(self) -> mpsc::Receiver<Result<ws::Frame, WsError<()>>> {
        let (tx, rx): (_, mpsc::Receiver<Result<ws::Frame, WsError<()>>>) = mpsc::channel();

        rt::spawn(async move {
            let tx2 = tx.clone();
            let io = self.io.get_ref();

            let result = self
                .start(fn_service(async move |item: ws::Frame| {
                    match tx.send(Ok(item)) {
                        Ok(()) => (),
                        Err(_) => io.close(),
                    }
                    Ok::<Option<ws::Message>, ()>(None)
                }))
                .await;

            if let Err(e) = result {
                let _ = tx2.send(Err(e));
            }
        });

        rx
    }

    /// Runs the WebSocket dispatcher with `svc` handling received frames.
    ///
    /// The service may return a message to send to the peer or [`None`] when no
    /// response is required.
    pub async fn start<T>(
        self,
        svc: impl IntoService<T, (), ws::Frame>,
    ) -> Result<(), WsError<T::Error>>
    where
        T: Service<(), ws::Frame, Res = Option<ws::Message>> + 'static,
    {
        let io = self.io.get_ref();
        let sink = self.sink();
        let service = apply_fn(
            svc.into_service().map_err(WsError::Service),
            async move |req, svc| match req {
                DispatchItem::<ws::Codec>::Item(item) => {
                    let close = matches!(item, ws::Frame::Close(_));
                    let result = svc.call(item).await;
                    if matches!(&result, Ok(Some(ws::Message::Close(_)))) {
                        sink.start_close_timeout();
                    }
                    if close {
                        let io = io.clone();
                        rt::spawn(async move { io.close() });
                    }
                    result
                }
                DispatchItem::Control(_) => Ok(None),
                DispatchItem::Stop(Reason::Service) => {
                    Ok(Some(ws::Message::Close(Some(CloseReason {
                        code: CloseCode::Away,
                        description: None,
                    }))))
                }
                DispatchItem::Stop(Reason::KeepAliveTimeout) => Err(WsError::KeepAlive),
                DispatchItem::Stop(Reason::ReadTimeout) => Err(WsError::ReadTimeout),
                DispatchItem::Stop(Reason::WriteTimeout) => Err(WsError::WriteTimeout),
                DispatchItem::Stop(Reason::Decoder(e) | Reason::Encoder(e)) => {
                    Err(WsError::Protocol(e))
                }
                DispatchItem::Stop(Reason::Io(e)) => Err(WsError::Disconnected(e)),
            },
        );

        Dispatcher::new(self.io, self.codec, Pipeline::new((), service)).await
    }
}

impl<F: Filter> WsConnection<F> {
    /// Erases the concrete I/O filter type.
    pub fn seal(self) -> WsConnection<Sealed> {
        WsConnection {
            io: self.io.seal(),
            codec: self.codec,
            res: self.res,
        }
    }

    /// Converts the connection into a binary WebSocket transport.
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
        let client = WsClient::new("http://localhost", SharedCfg::default());
        assert!(format!("{client:?}").contains("WsClient"));
    }

    #[crate::rt_test]
    async fn header_override() {
        let cfg = WsClientConfig::new()
            .set_header(header::CONTENT_TYPE, "111")
            .unwrap()
            .set_header(header::CONTENT_TYPE, "222")
            .unwrap();

        assert_eq!(
            cfg.headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "222"
        );
    }

    #[test]
    fn protocols() {
        let cfg = WsClientConfig::new()
            .set_protocols(["chat", "superchat"])
            .unwrap();
        assert_eq!(
            cfg.headers
                .get(header::SEC_WEBSOCKET_PROTOCOL)
                .unwrap()
                .to_str()
                .unwrap(),
            "chat,superchat"
        );

        let cfg = cfg.set_protocols([] as [&str; 0]).unwrap();
        assert!(!cfg.headers.contains_key(header::SEC_WEBSOCKET_PROTOCOL));
        assert!(WsClientConfig::new().set_protocols(["bad\n"]).is_err());
        assert!(
            WsClientConfig::new()
                .set_protocols(["bad protocol"])
                .is_err()
        );
        assert!(
            WsClientConfig::new()
                .set_protocols(["first,second"])
                .is_err()
        );
    }

    #[test]
    fn negotiation() {
        let configured = WsClientConfig::new()
            .set_protocols(["chat", "superchat"])
            .unwrap();
        let mut response = HeaderMap::new();

        response.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat"),
        );
        validate_negotiation(&response, &configured.headers).unwrap();

        let mut offered_headers = HeaderMap::new();
        offered_headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat, superchat"),
        );
        response.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("superchat"),
        );
        validate_negotiation(&response, &offered_headers).unwrap();

        response.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("other"),
        );
        assert!(matches!(
            validate_negotiation(&response, &configured.headers),
            Err(WsClientError::InvalidWebSocketProtocol(_))
        ));

        response.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat,superchat"),
        );
        assert!(matches!(
            validate_negotiation(&response, &configured.headers),
            Err(WsClientError::InvalidWebSocketProtocol(_))
        ));

        response.remove(header::SEC_WEBSOCKET_PROTOCOL);
        response.append(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("chat"),
        );
        response.append(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("superchat"),
        );
        assert!(matches!(
            validate_negotiation(&response, &configured.headers),
            Err(WsClientError::InvalidWebSocketProtocol(_))
        ));

        response.remove(header::SEC_WEBSOCKET_PROTOCOL);
        response.insert(
            header::SEC_WEBSOCKET_EXTENSIONS,
            HeaderValue::from_static("permessage-deflate"),
        );
        assert!(matches!(
            validate_negotiation(&response, &configured.headers),
            Err(WsClientError::UnexpectedWebSocketExtensions(_))
        ));
    }

    #[crate::rt_test]
    async fn basic_errs() {
        let err = WsClient::new("localhost", SharedCfg::default())
            .connect()
            .await
            .err()
            .unwrap();
        assert!(matches!(
            err.into_error(),
            WsClientError::Config(WsConfigError::MissingScheme)
        ));

        let err = WsClient::new("unknown://localhost", SharedCfg::default())
            .connect()
            .await
            .err()
            .unwrap();
        assert!(matches!(
            err.into_error(),
            WsClientError::Config(WsConfigError::UnknownScheme)
        ));

        let err = WsClient::new("/", SharedCfg::default())
            .connect()
            .await
            .err()
            .unwrap();
        assert!(matches!(
            err.into_error(),
            WsClientError::Config(WsConfigError::MissingHost)
        ));
    }

    #[crate::rt_test]
    async fn basic_auth() {
        let cfg = WsClientConfig::new()
            .set_basic_auth("username", Some("password"))
            .unwrap();
        assert_eq!(
            cfg.headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );

        let cfg = WsClientConfig::new()
            .set_basic_auth("username", None)
            .unwrap();
        assert_eq!(
            cfg.headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6"
        );

        let cfg = cfg.set_basic_auth("username", Some("password")).unwrap();
        assert_eq!(
            cfg.headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );
    }

    #[crate::rt_test]
    async fn bearer_auth() {
        let cfg = WsClientConfig::new()
            .set_bearer_auth("someS3cr3tAutht0k3n")
            .unwrap();
        assert_eq!(
            cfg.headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k3n"
        );
    }

    #[cfg(feature = "cookie")]
    #[crate::rt_test]
    async fn basics() {
        use coo_kie::Cookie;

        let cfg = WsClientConfig::new()
            .set_origin("test-origin")
            .unwrap()
            .set_max_frame_size(100)
            .set_server_mode()
            .set_protocols(["v1", "v2"])
            .unwrap()
            .set_header_if_none(header::CONTENT_TYPE, "json")
            .unwrap()
            .set_header_if_none(header::CONTENT_TYPE, "text")
            .unwrap()
            .set_cookie(Cookie::build(("cookie1", "value1")));

        assert!(cfg.server_mode);
        assert_eq!(cfg.max_size, 100);

        assert!(WsClient::new("/", SharedCfg::default()).err.is_some());
        assert!(
            WsClient::new("http:///test", SharedCfg::default())
                .err
                .is_some()
        );
        assert!(
            WsClient::new("hmm://test.com/", SharedCfg::default())
                .err
                .is_some()
        );
    }

    #[crate::rt_test]
    async fn pooled_request_head_method_is_get() {
        // a request head released back to the thread-local message pool keeps its
        // method (e.g. POST from the HTTP/1 server dispatcher); a ws client built
        // from such a recycled head must still send a GET handshake
        let mut head = Message::<RequestHead>::new();
        head.method = Method::POST;
        drop(head);
    }
}
