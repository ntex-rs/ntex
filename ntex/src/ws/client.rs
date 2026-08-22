//! Websockets client
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
use nanorand::{Rng, WyRand};

use crate::client::{ClientCodec, ClientConfig, ClientRawRequest, ClientResponse};
use crate::connect::{Connect, ConnectError, Connector};
use crate::error::{Error, ErrorMapping};
use crate::http::header::{self, HeaderValue};
use crate::http::{ConnectionType, Message, Method, RequestHead, StatusCode, Uri};
use crate::http::{body::BodySize, error::HttpError};
use crate::io::{Base, DispatchItem, Dispatcher, Filter, Io, Layer, Reason, Sealed};
use crate::service::{IntoService, Pipeline, apply_fn, fn_service};
use crate::{Cfg, Service, SharedCfg, channel::mpsc, rt, time::timeout, ws};

use super::error::{WsClientError, WsConfigError, WsError};
use super::{WsClientConfig, transport::WsTransport};

thread_local! {
    static CFG: SharedCfg = SharedCfg::new("WS-CLIENT").into();
}

/// `WebSocket` client builder
pub struct WsClient<F> {
    uri: Uri,
    cfg: Cfg<WsClientConfig>,
    http_cfg: Cfg<ClientConfig>,
    connector: Pipeline<Connect<Uri>, Io<F>, Error<ConnectError>>,
    filter: marker::PhantomData<F>,
}

impl WsClient<Base> {
    /// Set server uri
    pub fn new<U>(uri: U, cfg: impl Into<Cfg<WsClientConfig>>) -> Result<Self, WsConfigError>
    where
        Uri: TryFrom<U>,
        HttpError: From<<Uri as TryFrom<U>>::Error>,
    {
        let uri = Uri::try_from(uri).map_err(HttpError::from)?;

        // validate uri
        if uri.host().is_none() {
            return Err(WsConfigError::MissingHost);
        } else if uri.scheme().is_none() {
            return Err(WsConfigError::MissingScheme);
        } else if let Some(scheme) = uri.scheme() {
            match scheme.as_str() {
                "http" | "ws" | "https" | "wss" => (),
                _ => return Err(WsConfigError::UnknownScheme),
            }
        } else {
            return Err(WsConfigError::UnknownScheme);
        }

        let cfg = cfg.into();
        let shared = cfg.shared();

        Ok(WsClient {
            uri,
            cfg,
            http_cfg: shared.get(),
            connector: Pipeline::with_st(shared, Connector::<Uri>::new()),
            filter: marker::PhantomData,
        })
    }
}

impl<F> WsClient<F> {
    /// Create new websocket client
    pub fn connector<U, S>(self, f: impl IntoService<S, SharedCfg, Connect<Uri>>) -> WsClient<U>
    where
        U: Filter + 'static,
        S: Service<SharedCfg, Connect<Uri>, Res = Io<U>, Error = Error<ConnectError>> + 'static,
    {
        let shared = self.cfg.shared();
        WsClient {
            uri: self.uri,
            cfg: self.cfg,
            http_cfg: self.http_cfg,
            connector: Pipeline::with_st(shared, f.into_service()),
            filter: marker::PhantomData,
        }
    }

    #[cfg(feature = "openssl")]
    /// Use openssl connector.
    pub fn openssl(self, config: SslConnector) -> WsClient<Layer<openssl::SslFilter>> {
        self.connector(openssl::SslConnector::new(config))
    }

    #[cfg(feature = "rustls")]
    /// Use rustls connector.
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
    /// Complete request construction and connect to a websockets server.
    pub async fn connect(&self) -> Result<WsConnection<F>, Error<WsClientError>> {
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
            let val = HeaderValue::from_str(self.uri.host().unwrap()).unwrap();
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
        WyRand::new().fill(&mut sec_key);
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
            return Err(Error::from(WsClientError::InvalidUpgradeHeader));
        }

        // Check for "CONNECTION" header
        if let Some(conn) = response.headers.get(&header::CONNECTION) {
            if let Ok(s) = conn.to_str() {
                if !s.to_ascii_lowercase().contains("upgrade") {
                    log::trace!("{tag}: Invalid connection header: {s}");
                    return Err(Error::from(WsClientError::InvalidConnectionHeader(
                        conn.clone(),
                    )));
                }
            } else {
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
        log::trace!("{tag}: Ws handshake response verification is completed");

        // response and ws io
        Ok(WsConnection::new(
            io,
            ClientResponse::with_empty_payload(response, self.http_cfg.clone()),
            if self.cfg.server_mode {
                ws::Codec::new().max_size(self.cfg.max_size)
            } else {
                ws::Codec::new().max_size(self.cfg.max_size).client_mode()
            },
        ))
    }
}

impl<F> fmt::Debug for WsClient<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WsClient").field("cfg", &self.cfg).finish()
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

    /// Start client websockets service.
    pub async fn start<T>(
        self,
        svc: impl IntoService<T, (), ws::Frame>,
    ) -> Result<(), WsError<T::Error>>
    where
        T: Service<(), ws::Frame, Res = Option<ws::Message>> + 'static,
    {
        let service = apply_fn(
            svc.into_service().map_err(WsError::Service),
            async move |req, svc| match req {
                DispatchItem::<ws::Codec>::Item(item) => svc.call(item).await,
                DispatchItem::Control(_) => Ok(None),
                DispatchItem::Stop(Reason::KeepAliveTimeout) => Err(WsError::KeepAlive),
                DispatchItem::Stop(Reason::ReadTimeout) => Err(WsError::ReadTimeout),
                DispatchItem::Stop(Reason::Decoder(e) | Reason::Encoder(e)) => {
                    Err(WsError::Protocol(e))
                }
                DispatchItem::Stop(Reason::Io(e)) => Err(WsError::Disconnected(e)),
            },
        );

        Dispatcher::new(self.io, self.codec, Pipeline::new(service)).await
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

    #[crate::rt_test]
    async fn basic_errs() {
        let err = WsClient::new("localhost", SharedCfg::default())
            .err()
            .unwrap();
        assert!(matches!(err, WsConfigError::MissingScheme));

        let err = WsClient::new("unknown://localhost", SharedCfg::default())
            .err()
            .unwrap();
        assert!(matches!(err, WsConfigError::UnknownScheme));

        let err = WsClient::new("/", SharedCfg::default()).err().unwrap();
        assert!(matches!(err, WsConfigError::MissingHost));
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
            .set_header_if_unset(header::CONTENT_TYPE, "json")
            .unwrap()
            .set_header_if_unset(header::CONTENT_TYPE, "text")
            .unwrap()
            .set_cookie(Cookie::build(("cookie1", "value1")));

        assert!(cfg.server_mode);
        assert_eq!(cfg.max_size, 100);

        assert!(WsClient::new("/", SharedCfg::default()).is_err());
        assert!(WsClient::new("http:///test", SharedCfg::default()).is_err());
        assert!(WsClient::new("hmm://test.com/", SharedCfg::default()).is_err());
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
