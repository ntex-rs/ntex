use std::{fmt, net};

use base64::{Engine, engine::general_purpose::STANDARD as base64};
#[cfg(feature = "cookie")]
use coo_kie::{Cookie, CookieJar};

use crate::http::error::HttpError;
use crate::http::header::{self, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use crate::service::cfg::{CfgContext, Configuration};
use crate::time::Millis;

/// `WebSocket` client builder
#[derive(Debug)]
pub struct WsClientConfig {
    pub(super) addr: Option<net::SocketAddr>,
    pub(super) max_size: usize,
    pub(super) timeout: Millis,
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
    /// Create instance of `WsClientConfig`.
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
            #[cfg(feature = "cookie")]
            cookies: None,
            config: CfgContext::default(),
        }
    }

    #[must_use]
    /// Set socket address of the server.
    ///
    /// This address is used for connection. If address is not
    /// provided url's host name get resolved.
    pub fn set_address(mut self, addr: net::SocketAddr) -> Self {
        self.addr = Some(addr);
        self
    }

    #[must_use]
    /// Set supported websocket protocols.
    pub fn set_protocols<U, V>(mut self, protos: U) -> Self
    where
        U: IntoIterator<Item = V>,
        V: AsRef<str>,
    {
        let mut protos = protos
            .into_iter()
            .fold(String::new(), |acc, s| acc + s.as_ref() + ",");
        protos.pop();

        self.headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::try_from(protos.as_str()).unwrap(),
        );
        self
    }

    #[must_use]
    #[cfg(feature = "cookie")]
    /// Set a cookie.
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

    /// Set request Origin.
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
    /// Set max frame size.
    ///
    /// By default max size is set to 64kb
    pub fn set_max_frame_size(mut self, size: usize) -> Self {
        self.max_size = size;
        self
    }

    #[must_use]
    /// Disable payload masking.
    ///
    /// By default ws client masks frame payload.
    pub fn set_server_mode(mut self) -> Self {
        self.server_mode = true;
        self
    }

    /// Append a header.
    ///
    /// Header gets appended to existing header.
    /// To override header use `set_header()` method.
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

    /// Insert a header only if it is not yet set.
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

    /// Set HTTP basic authorization header.
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

    /// Set HTTP bearer authentication header.
    pub fn set_bearer_auth(self, token: impl fmt::Display) -> Result<Self, HttpError> {
        self.set_header(AUTHORIZATION, format!("Bearer {token}"))
    }

    #[must_use]
    /// Set request timeout.
    ///
    /// Request timeout is the total time before a response must be received.
    /// Default value is 5 seconds.
    pub fn set_timeout(mut self, timeout: impl Into<Millis>) -> Self {
        self.timeout = timeout.into();
        self
    }
}
