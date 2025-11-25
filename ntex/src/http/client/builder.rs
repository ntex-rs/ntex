use std::{fmt, rc::Rc};

use base64::{Engine, engine::general_purpose::STANDARD as base64};

use crate::http::error::HttpError;
use crate::http::header::{self, HeaderName, HeaderValue};
use crate::{io::SharedConfig, service::ServiceFactory, time::Millis};

use super::error::ClientBuilderError;
use super::{Client, ClientConfig, ClientInner, Connector};

/// An HTTP Client builder
///
/// This type can be used to construct an instance of `Client` through a
/// builder-like pattern.
#[derive(Debug)]
pub struct ClientBuilder {
    default_headers: bool,
    allow_redirects: bool,
    max_redirects: usize,
    config: ClientConfig,
    connector: Connector,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientBuilder {
    pub fn new() -> Self {
        ClientBuilder {
            default_headers: true,
            allow_redirects: true,
            max_redirects: 10,
            config: ClientConfig::default(),
            connector: Connector::default(),
        }
    }

    /// Use custom connector service.
    pub fn connector<T>(mut self, connector: Connector) -> Self {
        self.connector = connector;
        self
    }

    /// Set response timeout.
    ///
    /// Response timeout is the total time before a response must be received.
    /// Default value is 5 seconds.
    pub fn response_timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.config.timeout = timeout.into();
        self
    }

    /// Disable response timeout.
    pub fn disable_timeout(mut self) -> Self {
        self.config.timeout = Millis::ZERO;
        self
    }

    /// Do not follow redirects.
    ///
    /// Redirects are allowed by default.
    pub fn disable_redirects(mut self) -> Self {
        self.allow_redirects = false;
        self
    }

    /// Set max number of redirects.
    ///
    /// Max redirects is set to 10 by default.
    pub fn max_redirects(mut self, num: usize) -> Self {
        self.max_redirects = num;
        self
    }

    /// Do not add default request headers.
    /// By default `Date` and `User-Agent` headers are set.
    pub fn no_default_headers(mut self) -> Self {
        self.default_headers = false;
        self
    }

    /// Max size of response payload.
    /// By default max size is 256Kb
    pub fn response_payload_limit(mut self, limit: usize) -> Self {
        self.config.response_pl_limit = limit;
        self
    }

    /// Set response timeout.
    ///
    /// Response payload timeout is the total time before a payload must be received.
    /// Default value is 10 seconds.
    pub fn response_payload_timeout(mut self, timeout: Millis) -> Self {
        self.config.response_pl_timeout = timeout;
        self
    }

    /// Add default header. Headers added by this method
    /// get added to every request.
    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        HeaderValue: TryFrom<V>,
        <HeaderName as TryFrom<K>>::Error: fmt::Debug + Into<HttpError>,
        <HeaderValue as TryFrom<V>>::Error: fmt::Debug + Into<HttpError>,
    {
        match HeaderName::try_from(key) {
            Ok(key) => match HeaderValue::try_from(value) {
                Ok(value) => {
                    self.config.headers.append(key, value);
                }
                Err(e) => log::error!("Header value error: {e:?}"),
            },
            Err(e) => log::error!("Header name error: {e:?}"),
        }
        self
    }

    /// Set client wide HTTP basic authorization header
    pub fn basic_auth<U>(self, username: U, password: Option<&str>) -> Self
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => format!("{username}:"),
        };
        self.header(
            header::AUTHORIZATION,
            format!("Basic {}", base64.encode(auth)),
        )
    }

    /// Set client wide HTTP bearer authentication header
    pub fn bearer_auth<T>(self, token: T) -> Self
    where
        T: fmt::Display,
    {
        self.header(header::AUTHORIZATION, format!("Bearer {token}"))
    }

    /// Finish build process and create `Client` instance.
    pub async fn finish(self, cfg: SharedConfig) -> Result<Client, ClientBuilderError> {
        self.connector
            .create(cfg)
            .await
            .map_err(|_| ClientBuilderError::ConnectorFailed)
            .map(|connector| {
                Client(Rc::new(ClientInner {
                    config: Rc::new(self.config),
                    connector: connector.into(),
                }))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[crate::rt_test]
    async fn basics() {
        let builder = ClientBuilder::new()
            .disable_timeout()
            .disable_redirects()
            .max_redirects(10)
            .no_default_headers();
        assert!(!builder.allow_redirects);
        assert!(!builder.default_headers);
        assert_eq!(builder.max_redirects, 10);
    }

    #[crate::rt_test]
    async fn response_payload_limit() {
        let builder = ClientBuilder::default();
        assert_eq!(builder.config.response_pl_limit, 262_144);

        let builder = builder.response_payload_limit(10);
        assert_eq!(builder.config.response_pl_limit, 10);
    }

    #[crate::rt_test]
    async fn response_payload_timeout() {
        let builder = ClientBuilder::default();
        assert_eq!(builder.config.response_pl_timeout, Millis(10_000));

        let builder = builder.response_payload_timeout(Millis(10));
        assert_eq!(builder.config.response_pl_timeout, Millis(10));
    }

    #[crate::rt_test]
    async fn valid_header_name() {
        let builder = ClientBuilder::new().header("Content-Length", 1);
        assert!(builder.config.headers.contains_key("Content-Length"));
    }

    #[crate::rt_test]
    async fn invalid_header_name() {
        let builder = ClientBuilder::new().header("no valid header name", 1);
        assert!(!builder.config.headers.contains_key("no valid header name"));
    }

    #[crate::rt_test]
    async fn valid_header_value() {
        let valid_header_value = HeaderValue::from(1234);
        let builder = ClientBuilder::new().header("Content-Length", &valid_header_value);
        assert_eq!(
            builder.config.headers.get("Content-Length"),
            Some(&valid_header_value)
        );
    }

    #[crate::rt_test]
    async fn invalid_header_value() {
        let builder = ClientBuilder::new().header("Content-Length", "\n");
        assert!(!builder.config.headers.contains_key("Content-Length"));
    }

    #[crate::rt_test]
    async fn client_basic_auth() {
        let client = ClientBuilder::new().basic_auth("username", Some("password"));
        assert_eq!(
            client
                .config
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6cGFzc3dvcmQ="
        );

        let client = ClientBuilder::new().basic_auth("username", None);
        assert_eq!(
            client
                .config
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Basic dXNlcm5hbWU6"
        );
    }

    #[crate::rt_test]
    async fn client_bearer_auth() {
        let client = ClientBuilder::new().bearer_auth("someS3cr3tAutht0k3n");
        assert_eq!(
            client
                .config
                .headers
                .get(header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer someS3cr3tAutht0k3n"
        );
    }
}
