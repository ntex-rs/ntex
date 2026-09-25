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
    pub(super) h1_lifetime: Duration,
    pub(super) h1_keep_alive: Duration,
    pub(super) h1_limit: usize,
    pub(super) h2_lifetime: Duration,
    pub(super) h2_keep_alive: Duration,
    pub(super) h2_limit: usize,
    pub(super) h2_max_streams: u32,

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
            h1_lifetime: Duration::from_secs(75),
            h1_keep_alive: Duration::from_secs(15),
            h1_limit: 8,
            h2_lifetime: Duration::from_secs(3600),
            h2_keep_alive: Duration::from_secs(60),
            h2_limit: 16,
            h2_max_streams: 100,

            config: CfgContext::default(),
        }
    }

    /// Returns the headers added to every request.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns the response-header timeout.
    pub fn response_timeout(&self) -> Millis {
        self.timeout
    }

    /// Returns the maximum response payload size.
    ///
    /// A value of zero disables the limit.
    pub fn response_payload_limit(&self) -> usize {
        self.pl_limit
    }

    /// Returns the timeout for reading a complete response payload.
    pub fn response_payload_timeout(&self) -> Millis {
        self.pl_timeout
    }

    /// Returns the maximum number of simultaneous HTTP/1 connections per connection pool.
    pub fn h1_connection_limit(&self) -> usize {
        self.h1_limit
    }

    /// Returns the maximum number of HTTP/2 connections per host.
    pub fn h2_connection_limit(&self) -> usize {
        self.h2_limit
    }

    /// Returns the maximum number of concurrent requests per HTTP/2 connection.
    pub fn h2_max_streams(&self) -> u32 {
        self.h2_max_streams
    }

    /// Returns the keep-alive period for idle HTTP/2 connections.
    pub fn h2_keepalive(&self) -> Seconds {
        Seconds(self.h2_keep_alive.as_secs().try_into().unwrap_or(u16::MAX))
    }

    /// Returns the maximum lifetime of an HTTP/2 connection.
    pub fn h2_lifetime(&self) -> Seconds {
        Seconds(self.h2_lifetime.as_secs().try_into().unwrap_or(u16::MAX))
    }

    #[must_use]
    /// Sets the maximum number of simultaneous connections per connection pool.
    ///
    /// The limit is shared by all hosts. A client keeps separate pools for
    /// plain and TLS connections, and each pool has its own limit. The limit
    /// counts HTTP/1 connections in use and connections being opened;
    /// HTTP/2 connections are limited by
    /// [`set_h2_connection_limit`](Self::set_h2_connection_limit). A value of
    /// zero disables the limit. The default is 8.
    pub fn set_h1_connection_limit(mut self, limit: usize) -> Self {
        self.h1_limit = limit;
        self
    }

    #[must_use]
    /// Sets the keep-alive period for idle pooled HTTP/1 connections.
    ///
    /// HTTP/2 connections use [`set_h2_keepalive`](Self::set_h2_keepalive).
    /// A pooled connection that has been idle longer than this period is not
    /// reused. Expiration is checked lazily, when a connection for the same
    /// host is next requested; the expired connection is closed at that point.
    /// The default is 15 seconds.
    pub fn set_h1_keepalive<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.h1_keep_alive = dur.into().into();
        self
    }

    #[must_use]
    /// Sets the maximum lifetime of a pooled HTTP/1 connection.
    ///
    /// HTTP/2 connections use [`set_h2_lifetime`](Self::set_h2_lifetime).
    /// A connection older than this period is not reused, regardless of how
    /// recently it was used. Like the keep-alive period, this is checked when a
    /// connection for the same host is next requested. The default is 75 seconds.
    pub fn set_h1_lifetime<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.h1_lifetime = dur.into().into();
        self
    }

    #[must_use]
    /// Sets the maximum number of HTTP/2 connections per host.
    ///
    /// HTTP/2 connections are shared by concurrent requests. A new connection
    /// to a host is opened only when every existing HTTP/2 connection to that
    /// host has reached its stream limit, see
    /// [`set_h2_max_streams`](Self::set_h2_max_streams). When the limit is
    /// reached, requests wait for a free stream.
    ///
    /// Requests on established HTTP/2 connections do not count against
    /// [`set_h1_connection_limit`](Self::set_h1_connection_limit); opening a new
    /// connection does, because the protocol is not known until the
    /// connection is established. A value of zero disables the limit.
    /// The default is 16.
    pub fn set_h2_connection_limit(mut self, limit: usize) -> Self {
        self.h2_limit = limit;
        self
    }

    #[must_use]
    /// Sets the maximum number of concurrent requests per HTTP/2 connection.
    ///
    /// The peer's `SETTINGS_MAX_CONCURRENT_STREAMS` also applies; the lower of
    /// the two is used. A value of zero uses only the peer's setting.
    /// The default is 100.
    pub fn set_h2_max_streams(mut self, limit: u32) -> Self {
        self.h2_max_streams = limit;
        self
    }

    #[must_use]
    /// Sets the keep-alive period for idle HTTP/2 connections.
    ///
    /// An HTTP/2 connection is idle when it has no in-flight requests; the
    /// period is measured from the completion of its last request, including
    /// the response payload. An idle connection older than this period is
    /// closed when a connection for the same host is next requested.
    /// A zero duration disables the idle check. The default is 60 seconds.
    pub fn set_h2_keepalive<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.h2_keep_alive = dur.into().into();
        self
    }

    #[must_use]
    /// Sets the maximum lifetime of an HTTP/2 connection.
    ///
    /// An HTTP/2 connection older than this period is not used for new
    /// requests and is closed gracefully, after its in-flight requests
    /// complete. This is checked when a connection for the same host is next
    /// requested. A zero duration disables the limit. The default is 1 hour.
    pub fn set_h2_lifetime<T: Into<Seconds>>(mut self, dur: T) -> Self {
        self.h2_lifetime = dur.into().into();
        self
    }

    #[must_use]
    /// Sets the response-header timeout.
    ///
    /// The timeout covers receiving the response head after the request has
    /// been sent. Connecting and sending the request are not included. A zero
    /// duration disables the timeout. The default is 5 seconds.
    pub fn set_response_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.timeout = timeout.into();
        self
    }

    #[must_use]
    /// Disables the response-header timeout.
    ///
    /// This is the same as `set_response_timeout(Millis::ZERO)`.
    pub fn disable_timeout(mut self) -> Self {
        self.timeout = Millis::ZERO;
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
    pub fn set_response_payload_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.pl_timeout = timeout.into();
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
        let cfg = ClientConfig::new().disable_timeout();
        assert_eq!(cfg.timeout, Millis::ZERO);
    }

    #[test]
    fn h2_settings() {
        let cfg = ClientConfig::new();
        assert_eq!(cfg.h2_connection_limit(), 16);
        assert_eq!(cfg.h2_max_streams(), 100);
        assert_eq!(cfg.h2_keepalive(), Seconds(60));
        assert_eq!(cfg.h2_lifetime(), Seconds(3600));

        let cfg = cfg
            .set_h2_connection_limit(2)
            .set_h2_max_streams(10)
            .set_h2_keepalive(Seconds(5))
            .set_h2_lifetime(Seconds(50));
        assert_eq!(cfg.h2_connection_limit(), 2);
        assert_eq!(cfg.h2_max_streams(), 10);
        assert_eq!(cfg.h2_keepalive(), Seconds(5));
        assert_eq!(cfg.h2_lifetime(), Seconds(50));
        // http/1 settings are not affected
        assert_eq!(cfg.h1_connection_limit(), 8);
        assert_eq!(cfg.h1_keep_alive, Duration::from_secs(15));
        assert_eq!(cfg.h1_lifetime, Duration::from_secs(75));
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
