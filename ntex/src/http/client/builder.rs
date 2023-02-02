use std::{fmt, rc::Rc};

use base64::{engine::general_purpose::STANDARD as base64, Engine};

use crate::http::error::HttpError;
use crate::http::header::{self, HeaderMap, HeaderName, HeaderValue};
use crate::{service::Service, time::Millis};

use super::connect::ConnectorWrapper;
use super::error::ConnectError;
use super::{Client, ClientConfig, Connect, Connection, Connector};

/// An HTTP Client builder
///
/// This type can be used to construct an instance of `Client` through a
/// builder-like pattern.
pub struct ClientBuilder {
    config: ClientConfig,
    default_headers: bool,
    allow_redirects: bool,
    max_redirects: usize,
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
            config: ClientConfig {
                headers: HeaderMap::new(),
                timeout: Millis(5_000),
                connector: Box::new(ConnectorWrapper(Connector::default().finish())),
            },
        }
    }

    /// Use custom connector service.
    pub fn connector<T>(mut self, connector: T) -> Self
    where
        T: Service<Connect, Response = Connection, Error = ConnectError> + 'static,
    {
        self.config.connector = Box::new(ConnectorWrapper(connector));
        self
    }

    /// Set request timeout.
    ///
    /// Request timeout is the total time before a response must be received.
    /// Default value is 5 seconds.
    pub fn timeout<T: Into<Millis>>(mut self, timeout: T) -> Self {
        self.config.timeout = timeout.into();
        self
    }

    /// Disable request timeout.
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
                Err(e) => log::error!("Header value error: {:?}", e),
            },
            Err(e) => log::error!("Header name error: {:?}", e),
        }
        self
    }

    /// Set client wide HTTP basic authorization header
    pub fn basic_auth<U>(self, username: U, password: Option<&str>) -> Self
    where
        U: fmt::Display,
    {
        let auth = match password {
            Some(password) => format!("{}:{}", username, password),
            None => format!("{}:", username),
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
        self.header(header::AUTHORIZATION, format!("Bearer {}", token))
    }

    /// Finish build process and create `Client` instance.
    pub fn finish(self) -> Client {
        Client(Rc::new(self.config))
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
