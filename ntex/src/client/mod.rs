//! HTTP Client
//!
//! ```rust
//! use ntex::client::Client;
//!
//! #[ntex::main]
//! async fn main() {
//!    let mut client = Client::new();
//!
//!    let response = client.get("http://www.rust-lang.org") // <- Create request builder
//!        .header("User-Agent", "ntex::web")
//!        .send()                                           // <- Send http request
//!        .await;
//!
//!     println!("Response: {:?}", response);
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
pub use self::connection::Connection;
pub use self::request::ClientRequest;
pub use self::response::{ClientResponse, JsonBody, MessageBody};
pub use self::service::{ServiceRequest, ServiceResponse};
pub use self::test::TestResponse;

pub(crate) use self::codec::{ClientCodec, ClientPayloadCodec};
use crate::client::error::ConnectError;
use crate::http::{HeaderMap, Method, RequestHead, Uri, body::BodySize, error::HttpError};
use crate::service::{cfg::SharedCfg, pipeline::PipelineState};
use crate::{Cfg, Pipeline, error::Error, io::IoBoxed};

type ConnectorPipeline = PipelineState<SharedCfg, Connect, IoBoxed, Error<ConnectError>>;

#[derive(Debug, Clone)]
pub struct Connect {
    pub uri: Uri,
    pub addr: Option<std::net::SocketAddr>,
}

/// An HTTP Client
///
/// ```rust
/// use ntex::client::Client;
///
/// #[ntex::main]
/// async fn main() {
///     let mut client = Client::new();
///
///     let res = client.get("http://www.rust-lang.org") // <- Create request builder
///         .header("User-Agent", "ntex::web")
///         .send()                             // <- Send http request
///         .await;                             // <- send request and wait for response
///
///      println!("Response: {:?}", res);
/// }
/// ```
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
    /// Create new client instance with default settings.
    pub fn new() -> Client {
        ClientBuilder::new().build(SharedCfg::default())
    }

    /// Build client instance.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Create new client instance with configuration.
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

    /// Returns when the client is ready to process requests.
    pub async fn ready(&self) -> Result<(), Error<error::ClientError>> {
        self.svc.ready().await
    }

    /// Construct HTTP request.
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

    /// Create `ClientRequest` from `RequestHead`
    ///
    /// It is useful for proxy requests. This implementation
    /// copies all headers and the method.
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
