use std::{fmt, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as base64};

use crate::http::header::{self, HeaderName, HeaderValue};
use crate::http::{HeaderMap, error::HttpError};
use crate::service::cfg::{CfgContext, Configuration};
use crate::time::{Millis, Seconds};

#[derive(Debug)]
/// Runtime configuration for an HTTP [`Client`](super::Client).
///
/// The configuration is stored in [`SharedCfg`](crate::SharedCfg) and can be
/// supplied before constructing a client with [`Client::with_config`](super::Client::with_config)
/// or [`ClientBuilder::build`](super::ClientBuilder::build).
pub struct ClientConfig {
    pub(super) headers: HeaderMap,
    pub(super) timeout: Millis,
    pub(super) pl_limit: usize,
    pub(super) pl_timeout: Millis,
    pub(super) default_headers: bool,
    pub(super) allow_redirects: bool,
    pub(super) max_redirects: usize,
    pub(super) conn_lifetime: Duration,
    pub(super) conn_keep_alive: Duration,
    pub(super) limit: usize,

    config: CfgContext,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl Configuration for ClientConfig {
    const NAME: &str = "Http client configuration";

    fn ctx(&self) -> &CfgContext {
        &self.config
    }

    fn set_ctx(&mut self, ctx: CfgContext) {
        self.config = ctx;
    }
}

impl ClientConfig {
    #[must_use]
    /// Creates an HTTP client configuration with default values.
    pub fn new() -> ClientConfig {
        ClientConfig {
            headers: HeaderMap::new(),
            timeout: Millis(5_000),
            pl_limit: 262_144,
            pl_timeout: Millis(10_000),
            default_headers: true,
            allow_redirects: true,
            max_redirects: 2,
            conn_lifetime: Duration::from_secs(75),
            conn_keep_alive: Duration::from_secs(15),
            limit: 8,

            config: CfgContext::default(),
        }
    }

    /// Returns the headers added to every request.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns the response-header timeout.
    pub fn timeout(&self) -> Millis {
        self.timeout
    }

    /// Returns the maximum response payload size.
    ///
    /// A value of zero disables the limit.
    pub fn payload_limit(&self) -> usize {
        self.pl_limit
    }

    /// Returns the timeout for reading a complete response payload.
    pub fn payload_timeout(&self) -> Millis {
        self.pl_timeout
    }

    #[must_use]
    /// Sets the maximum number of simultaneous connections per scheme.
    ///
    /// A value of zero disables the limit. The default is 8.
    pub fn set_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    #[must_use]
    /// Sets the keep-alive period for idle pooled connections.
    ///
    /// A pooled connection is closed when it has been idle longer than this
    /// period. The default is 15 seconds.
    pub fn set_keep_alive<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.conn_keep_alive = dur.into().into();
        self
    }

    #[must_use]
    /// Sets the maximum lifetime of a pooled connection.
    ///
    /// A connection is closed after this period regardless of how recently it
    /// was used. The default is 75 seconds.
    pub fn set_lifetime<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.conn_lifetime = dur.into().into();
        self
    }

    #[must_use]
    /// Sets the response-header timeout.
    ///
    /// The timeout covers sending the request and receiving the response head
    /// after a connection has been acquired. The default is 5 seconds.
    pub fn set_response_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.timeout = timeout.into();
        self
    }

    #[must_use]
    /// Disables the response-header timeout.
    pub fn disable_timeout(mut self) -> Self {
        self.timeout = Millis::ZERO;
        self
    }

    #[must_use]
    /// Retains the compatibility setting for disabling redirects.
    ///
    /// The built-in client sender does not currently follow redirects, so this
    /// setting has no effect.
    pub fn disable_redirects(mut self) -> Self {
        self.allow_redirects = false;
        self
    }

    #[must_use]
    /// Retains the compatibility setting for the redirect limit.
    ///
    /// The stored default is 2. The built-in client sender does not currently
    /// follow redirects, so this setting has no effect.
    pub fn set_max_redirects(mut self, num: usize) -> Self {
        self.max_redirects = num;
        self
    }

    #[must_use]
    /// Retains the compatibility setting for automatic request headers.
    ///
    /// The built-in client does not automatically add `Date` or `User-Agent`
    /// headers, so this setting currently has no effect.
    pub fn set_no_default_headers(mut self) -> Self {
        self.default_headers = false;
        self
    }

    #[must_use]
    /// Sets the maximum size of a buffered response payload.
    ///
    /// The default is 256 KiB. A value of zero disables the limit.
    pub fn set_response_payload_limit(mut self, limit: usize) -> Self {
        self.pl_limit = limit;
        self
    }

    #[must_use]
    /// Sets the timeout for reading a complete response payload.
    ///
    /// The default is 10 seconds. A zero duration disables the timeout.
    pub fn set_response_payload_timeout(mut self, timeout: Millis) -> Self {
        self.pl_timeout = timeout;
        self
    }

    /// Adds a header to every request.
    ///
    /// A request-specific header with the same name takes precedence.
    pub fn set_header<K, V>(mut self, key: K, value: V) -> Result<Self, HttpError>
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: Into<HttpError>,
    {
        let key = HeaderName::try_from(key).map_err(Into::into)?;
        let value = HeaderValue::try_from(value).map_err(Into::into)?;
        self.headers.append(key, value);
        Ok(self)
    }

    /// Sets a client-wide HTTP Basic authentication header.
    pub fn set_basic_auth<U>(self, username: U, password: Option<&str>) -> Result<Self, HttpError>
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.set_header(
            header::AUTHORIZATION,
            format!("Basic {}", base64.encode(auth)),
        )
    }

    /// Sets a client-wide HTTP Bearer authentication header.
    pub fn set_bearer_auth<T>(self, token: T) -> Result<Self, HttpError>
    where
        T: fmt::Display,
    {
        self.set_header(header::AUTHORIZATION, format!("Bearer {token}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let cfg = ClientConfig::new()
            .disable_timeout()
            .disable_redirects()
            .set_max_redirects(10)
            .set_no_default_headers();
        assert!(!cfg.allow_redirects);
        assert!(!cfg.default_headers);
        assert_eq!(cfg.max_redirects, 10);
    }

    #[test]
    fn response_payload_limit() {
        let cfg = ClientConfig::new();
        assert_eq!(cfg.pl_limit, 262_144);

        let cfg = cfg.set_response_payload_limit(10);
        assert_eq!(cfg.pl_limit, 10);
    }

    #[test]
    fn response_payload_timeout() {
        let cfg = ClientConfig::default();
        assert_eq!(cfg.pl_timeout, Millis(10_000));

        let cfg = cfg.set_response_payload_timeout(Millis(10));
        assert_eq!(cfg.pl_timeout, Millis(10));
    }

    #[test]
    fn valid_header_name() {
        let cfg = ClientConfig::new().set_header("Content-Length", 1).unwrap();
        assert!(cfg.headers.contains_key("Content-Length"));
    }

    #[test]
    fn invalid_header_name() {
        let res = ClientConfig::new().set_header("no valid header name", 1);
        assert!(res.is_err());
    }

    #[test]
    fn valid_header_value() {
        let valid_header_value = HeaderValue::from(1234);
        let cfg = ClientConfig::new()
            .set_header("Content-Length", &valid_header_value)
            .unwrap();
        assert_eq!(cfg.headers.get("Content-Length"), Some(&valid_header_value));
    }

    #[test]
    fn invalid_header_value() {
        let res = ClientConfig::new()
            .set_header("Content-Length", "\n")
            .is_err();
        assert!(res);
    }

    #[test]
    fn client_basic_auth() {
        let cfg = ClientConfig::new()
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

        let cfg = ClientConfig::new()
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
    }

    #[test]
    fn client_bearer_auth() {
        let cfg = ClientConfig::new()
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
}
