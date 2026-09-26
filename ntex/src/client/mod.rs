//! HTTP client.
//!
//! ```rust,no_run
//! use ntex::client::Client;
//!
//! #[ntex::main]
//! async fn main() {
//!     let client = Client::new();
//!
//!     let response = client
//!         .get("https://www.rust-lang.org")
//!         .header("User-Agent", "ntex")
//!         .send()
//!         .await;
//!
//!     println!("Response: {response:?}");
//! }
//! ```
use std::rc::Rc;

mod builder;
mod cfg;
mod codec;
mod connection;
mod connector;
pub mod error;
mod h1proto;
mod h2proto;
mod pool;
mod request;
mod response;
mod sender;
mod service;
mod test;

pub use self::builder::ClientBuilder;
pub use self::cfg::ClientConfig;
pub use self::request::ClientRequest;
pub use self::response::{ClientResponse, JsonBody, MessageBody};
pub use self::service::{ServiceRequest, ServiceResponse};
pub use self::test::TestResponse;

pub(crate) use self::codec::{ClientCodec, ClientPayloadCodec};
pub(crate) use self::h1proto::host_header;
use crate::client::error::ConnectError;
use crate::http::{HeaderMap, Method, RequestHead, Uri, body::BodySize, error::HttpError};
use crate::service::{cfg::SharedCfg, pipeline::PipelineState};
use crate::{Cfg, Pipeline, error::Error, io::IoBoxed};

type ConnectorPipeline = PipelineState<SharedCfg, Connect, IoBoxed, Error<ConnectError>>;

#[derive(Debug, Clone)]
pub(crate) struct Connect {
    pub(crate) uri: Uri,
    pub(crate) addr: Option<std::net::SocketAddr>,
}

/// An HTTP client.
///
/// ```rust,no_run
/// use ntex::client::Client;
///
/// #[ntex::main]
/// async fn main() {
///     let client = Client::new();
///
///     let response = client
///         .get("https://www.rust-lang.org")
///         .header("User-Agent", "ntex")
///         .send()
///         .await;
///
///     println!("Response: {response:?}");
/// }
/// ```
///
/// # Shutdown
///
/// Clones of a client share its connection pools. A pool is stopped once the
/// last clone and every request created from it are dropped, and dropping does
/// not wait for connections to close:
///
/// - Requests waiting for a connection fail with
///   [`ConnectError::Disconnected`].
/// - Idle HTTP/1 connections are shut down in the background, bounded by the
///   I/O [shutdown timeout](crate::io::IoConfig::set_shutdown_timeout).
/// - HTTP/2 connections stop accepting requests and are closed gracefully in
///   the background once their in-flight requests have completed. Their
///   closing is not bounded by a timeout.
/// - Responses that are still being read keep their connections. An HTTP/1
///   connection is closed instead of being returned to the stopped pool.
/// - A connection that is still being established is closed once the
///   connect completes.
#[derive(Debug, Clone)]
pub struct Client {
    cfg: Cfg<ClientConfig>,
    svc: Rc<Pipeline<ServiceRequest, ServiceResponse, Error<error::ClientError>>>,
}

impl Default for Client {
    fn default() -> Self {
        Client::new()
    }
}

impl Client {
    /// Creates a client with default settings.
    pub fn new() -> Client {
        ClientBuilder::new().build(SharedCfg::default())
    }

    /// Creates a client builder.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Creates a client with shared service configuration.
    pub fn with_config(cfg: impl Into<SharedCfg>) -> Client {
        ClientBuilder::new().build(cfg.into())
    }

    pub(crate) fn with_service(
        cfg: Cfg<ClientConfig>,
        svc: Pipeline<ServiceRequest, ServiceResponse, Error<error::ClientError>>,
    ) -> Self {
        Client {
            cfg,
            svc: Rc::new(svc),
        }
    }

    /// Waits until the client is ready to process requests.
    pub async fn ready(&self) -> Result<(), Error<error::ClientError>> {
        self.svc.ready().await
    }

    /// Creates an HTTP request with the specified method and URL.
    pub fn request<U>(&self, method: Method, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        let mut req = ClientRequest::new(method, url, self.cfg.clone(), self.svc.bind());
        for (key, value) in self.cfg.headers() {
            req = req.set_header_if_none(key.clone(), value.clone());
        }
        req
    }

    /// Creates a [`ClientRequest`] from a [`RequestHead`].
    ///
    /// This is useful for proxy requests. The method and headers are copied
    /// from `head`; existing client default headers are not overwritten.
    pub fn request_from<U>(&self, url: U, head: &RequestHead) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        let mut req = self.request(head.method.clone(), url);
        for (key, value) in &head.headers {
            req = req.set_header_if_none(key.clone(), value.clone());
        }
        req
    }

    /// Construct HTTP *GET* request.
    pub fn get<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::GET, url)
    }

    /// Construct HTTP *HEAD* request.
    pub fn head<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::HEAD, url)
    }

    /// Construct HTTP *PUT* request.
    pub fn put<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::PUT, url)
    }

    /// Construct HTTP *POST* request.
    pub fn post<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::POST, url)
    }

    /// Construct HTTP *PATCH* request.
    pub fn patch<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::PATCH, url)
    }

    /// Construct HTTP *DELETE* request.
    pub fn delete<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::DELETE, url)
    }

    /// Construct HTTP *QUERY* request.
    pub fn query<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::QUERY, url)
    }

    /// Construct HTTP *OPTIONS* request.
    pub fn options<U>(&self, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        self.request(Method::OPTIONS, url)
    }
}

#[derive(Debug)]
pub(crate) struct ClientRawRequest {
    pub(crate) head: crate::http::Message<RequestHead>,
    pub(crate) headers: Option<HeaderMap>,
    pub(crate) size: BodySize,
}
