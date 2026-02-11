//! HTTP Client
//!
//! ```rust
//! use ntex::client::Client;
//!
//! #[ntex::main]
//! async fn main() {
//!    let mut client = Client::new().await;
//!
//!    let response = client.get("http://www.rust-lang.org") // <- Create request builder
//!        .header("User-Agent", "ntex::web")
//!        .send()                                           // <- Send http request
//!        .await;
//!
//!     println!("Response: {:?}", response);
//! }
//! ```
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
pub use self::connector::{Connector, ConnectorService};
pub use self::request::ClientRequest;
pub use self::response::{ClientResponse, JsonBody, MessageBody};
pub use self::service::{ServiceRequest, ServiceResponse};
pub use self::test::TestResponse;

pub(crate) use self::codec::{ClientCodec, ClientPayloadCodec};
use crate::http::{HeaderMap, Method, RequestHead, Uri, body::BodySize, error::HttpError};
use crate::{Pipeline, SharedCfg, service::boxed};

#[derive(Debug, Clone)]
pub struct Connect {
    pub uri: Uri,
    pub addr: Option<std::net::SocketAddr>,
}

type BoxedSender =
    boxed::BoxService<ServiceRequest, ServiceResponse, error::SendRequestError>;

/// An HTTP Client
///
/// ```rust
/// use ntex::client::Client;
///
/// #[ntex::main]
/// async fn main() {
///     let mut client = Client::new().await;
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
    svc: Pipeline<BoxedSender>,
    config: ClientConfig,
}

impl Client {
    /// Create new client instance with default settings.
    pub async fn new() -> Client {
        ClientBuilder::new()
            .build(SharedCfg::default())
            .await
            .unwrap()
    }

    /// Build client instance.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    pub(crate) fn with_service(svc: Pipeline<BoxedSender>, config: ClientConfig) -> Self {
        Client { svc, config }
    }

    /// Returns when the client is ready to process requests.
    pub async fn ready(&self) -> Result<(), error::SendRequestError> {
        self.svc.ready().await
    }

    /// Construct HTTP request.
    pub fn request<U>(&self, method: Method, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        let mut req = ClientRequest::new(method, url, self.svc.clone());

        for (key, value) in self.config.headers() {
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
