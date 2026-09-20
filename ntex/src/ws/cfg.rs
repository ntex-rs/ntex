use std::{fmt, net};

use base64::{Engine, engine::general_purpose::STANDARD as base64};
#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use crate::http::error::HttpError;
use crate::http::header::{
    self, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, InvalidHeaderValue,
};
use crate::service::cfg::{CfgContext, Configuration};
use crate::time::Millis;

/// Configuration for a WebSocket client connection.
///
/// Store this value in [`SharedCfg`](crate::SharedCfg) and pass the resulting
/// configuration to [`WsClient::new`](super::WsClient::new).
#[derive(Debug)]
pub struct WsClientConfig {
    pub(super) addr: Option<net::SocketAddr>,
    pub(super) max_size: usize,
    pub(super) timeout: Millis,
    pub(super) close_timeout: Millis,
    pub(super) headers: HeaderMap,
    pub(super) server_mode: bool,
    #[cfg(feature = "cookie")]
    pub(super) cookies: Option<CookieJar>,

    config: CfgContext,
}

impl Default for WsClientConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl Configuration for WsClientConfig {
    const NAME: &str = "WebSocket client configuration";

    fn ctx(&self) -> &CfgContext {
        &self.config
    }

    fn set_ctx(&mut self, ctx: CfgContext) {
        self.config = ctx;
    }
}

impl WsClientConfig {
    #[must_use]
    /// Creates a WebSocket client configuration with default values.
    pub fn new() -> WsClientConfig {
        let mut headers = HeaderMap::new();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(
            header::SEC_WEBSOCKET_VERSION,
            HeaderValue::from_static("13"),
        );

        WsClientConfig {
            headers,
            addr: None,
            max_size: 65_536,
            server_mode: false,
            timeout: Millis(5_000),
            close_timeout: Millis(5_000),
            #[cfg(feature = "cookie")]
            cookies: None,
            config: CfgContext::default(),
        }
    }

    #[must_use]
    /// Sets the server socket address.
    ///
    /// This address is used instead of resolving the URI host name.
    pub fn set_address(mut self, addr: net::SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    /// Sets the WebSocket subprotocols offered to the server.
    ///
    /// This replaces the current `Sec-WebSocket-Protocol` header. An empty
    /// iterator removes the header.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] if a protocol is not a valid HTTP token or the
    /// resulting list is not a valid HTTP header value.
    pub fn set_protocols<U, V>(mut self, protos: U) -> Result<Self, HttpError>
    where
        U: IntoIterator<Item = V>,
        V: AsRef<str>,
    {
        let mut values = Vec::new();
        for proto in protos {
            let proto = proto.as_ref();
            if !is_token(proto) {
                return Err(InvalidHeaderValue::default().into());
            }
            values.push(proto.to_owned());
        }
        let protos = values.join(",");

        if protos.is_empty() {
            self.headers.remove(header::SEC_WEBSOCKET_PROTOCOL);
        } else {
            self.headers.insert(
                header::SEC_WEBSOCKET_PROTOCOL,
                HeaderValue::try_from(protos.as_str())?,
            );
        }
        Ok(self)
    }

    #[must_use]
    #[cfg(feature = "cookie")]
    /// Adds a cookie to the opening handshake.
    pub fn set_cookie<C>(mut self, cookie: C) -> Self
    where
        C: Into<Cookie<'static>>,
    {
        if let Some(cookies) = &mut self.cookies {
            cookies.add(cookie.into());
        } else {
            let mut jar = CookieJar::new();
            jar.add(cookie.into());
            self.cookies = Some(jar);
        }
        self
    }

    /// Sets the `Origin` header for the opening handshake.
    pub fn set_origin<V, E>(mut self, origin: V) -> Result<Self, HttpError>
    where
        HeaderValue: TryFrom<V, Error = E>,
        HttpError: From<E>,
    {
        self.headers
            .insert(header::ORIGIN, HeaderValue::try_from(origin)?);
        Ok(self)
    }

    #[must_use]
    /// Sets the maximum accepted frame payload size.
    ///
    /// The default is 64 KiB.
    pub fn set_max_frame_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    #[must_use]
    /// Configures the connection to use server-side masking rules.
    ///
    /// By default, the client masks outgoing frames and expects unmasked
    /// incoming frames. Server mode reverses those rules.
    pub fn set_server_mode(mut self) -> Self {
        self.server_mode = true;
        self
    }

    /// Sets a header for the opening handshake.
    ///
    /// This replaces any existing value with the same name.
    pub fn set_header<K, V>(mut self, key: K, value: V) -> Result<Self, HttpError>
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        let key = HeaderName::try_from(key).map_err(Into::into)?;
        let value = HeaderValue::try_from(value).map_err(Into::into)?;
        self.headers.insert(key, value);
        Ok(self)
    }

    /// Sets a handshake header if it is not already present.
    pub fn set_header_if_unset<K, V>(mut self, key: K, value: V) -> Result<Self, HttpError>
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        let key = HeaderName::try_from(key).map_err(Into::into)?;
        if !self.headers.contains_key(&key) {
            self.headers
                .insert(key, HeaderValue::try_from(value).map_err(Into::into)?);
        }
        Ok(self)
    }

    /// Sets the HTTP Basic authentication header.
    pub fn set_basic_auth(
        self,
        username: impl fmt::Display,
        password: Option<&str>,
    ) -> Result<Self, HttpError> {
        let auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.set_header(AUTHORIZATION, format!("Basic {}", base64.encode(auth)))
    }

    /// Sets the HTTP bearer authentication header.
    pub fn set_bearer_auth(self, token: impl fmt::Display) -> Result<Self, HttpError> {
        self.set_header(AUTHORIZATION, format!("Bearer {token}"))
    }

    #[must_use]
    /// Sets the opening-handshake timeout.
    ///
    /// The timeout covers sending the upgrade request and receiving the
    /// response after a connection has been established. The default is
    /// 5 seconds. A zero duration disables the timeout.
    pub fn set_handshake_timeout(mut self, timeout: impl Into<Millis>) -> Self {
        self.timeout = timeout.into();
        self
    }

    #[must_use]
    /// Sets the closing-handshake timeout.
    ///
    /// After sending a close frame, the client waits this long for the peer's
    /// close response before shutting down the connection. The default is
    /// 5 seconds. A zero duration disables the timeout.
    pub fn set_close_timeout(mut self, timeout: impl Into<Millis>) -> Self {
        self.close_timeout = timeout.into();
        self
    }
}

pub(crate) fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}
