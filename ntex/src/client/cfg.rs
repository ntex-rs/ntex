use std::{fmt, time::Duration};

use base64::{Engine, engine::general_purpose::STANDARD as base64};

use crate::http::header::{self, HeaderName, HeaderValue};
use crate::http::{HeaderMap, error::HttpError};
use crate::service::cfg::{CfgContext, Configuration};
use crate::time::{Millis, Seconds};

#[derive(Debug)]
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
    /// Create instance of `HttpClientConfig`.
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

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn timeout(&self) -> Millis {
        self.timeout
    }

    pub fn payload_limit(&self) -> usize {
        self.pl_limit
    }

    pub fn payload_timeout(&self) -> Millis {
        self.pl_timeout
    }

    #[must_use]
    /// Set total number of simultaneous connections per type of scheme.
    ///
    /// If limit is 0, the connector has no limit.
    /// The default limit size is 8.
    pub fn set_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    #[must_use]
    /// Set keep-alive period for opened connection.
    ///
    /// Keep-alive period is the period between connection usage. If
    /// the delay between repeated usages of the same connection
    /// exceeds this period, the connection is closed.
    /// Default keep-alive period is 15 seconds.
    pub fn set_keep_alive<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.conn_keep_alive = dur.into().into();
        self
    }

    #[must_use]
    /// Set max lifetime period for connection.
    ///
    /// Connection lifetime is max lifetime of any opened connection
    /// until it is closed regardless of keep-alive period.
    /// Default lifetime period is 75 seconds.
    pub fn set_lifetime<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.conn_lifetime = dur.into().into();
        self
    }

    #[must_use]
    /// Set response timeout.
    ///
    /// Response timeout is the total time before a response must be received.
    /// Default value is 5 seconds.
    pub fn set_response_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.timeout = timeout.into();
        self
    }

    #[must_use]
    /// Disable response timeout.
    pub fn disable_timeout(mut self) -> Self {
        self.timeout = Millis::ZERO;
        self
    }

    #[must_use]
    /// Do not follow redirects.
    ///
    /// Redirects are allowed by default.
    pub fn disable_redirects(mut self) -> Self {
        self.allow_redirects = false;
        self
    }

    #[must_use]
    /// Set max number of redirects.
    ///
    /// Max redirects is set to 10 by default.
    pub fn set_max_redirects(mut self, num: usize) -> Self {
        self.max_redirects = num;
        self
    }

    #[must_use]
    /// Do not add default request headers.
    ///
    /// By default `Date` and `User-Agent` headers are set.
    pub fn set_no_default_headers(mut self) -> Self {
        self.default_headers = false;
        self
    }

    #[must_use]
    /// Max size of response payload.
    ///
    /// By default max size is 256Kb
    pub fn set_response_payload_limit(mut self, limit: usize) -> Self {
        self.pl_limit = limit;
        self
    }

    #[must_use]
    /// Set response timeout.
    ///
    /// Response payload timeout is the total time before a payload must be received.
    /// Default value is 10 seconds.
    pub fn set_response_payload_timeout(mut self, timeout: Millis) -> Self {
        self.pl_timeout = timeout;
        self
    }

    /// Add default header.
    ///
    /// Headers added by this method get added to every request.
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

    /// Set client wide HTTP basic authorization header.
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

    /// Set client wide HTTP bearer authentication header.
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
