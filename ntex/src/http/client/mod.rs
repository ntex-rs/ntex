//! HTTP Client
//!
//! ```rust
//! use futures_util::future::{lazy, Future};
//! use ntex::http::client::Client;
//!
//! #[ntex::main]
//! async fn main() {
//!    let mut client = Client::default();
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
mod connect;
mod connection;
mod connector;
pub mod error;
mod frozen;
mod h1proto;
mod h2proto;
mod pool;
mod request;
mod response;
mod sender;
mod test;

pub use self::builder::ClientBuilder;
pub use self::connection::Connection;
pub use self::connector::Connector;
pub use self::frozen::{FrozenClientRequest, FrozenSendBuilder};
pub use self::request::ClientRequest;
pub use self::response::{ClientResponse, JsonBody, MessageBody};
pub use self::sender::SendClientRequest;
pub use self::test::TestResponse;

use crate::http::error::HttpError;
use crate::http::{HeaderMap, Method, RequestHead, Uri};
use crate::time::Millis;

use self::connect::{Connect as HttpConnect, ConnectorWrapper};

#[derive(Debug, Clone)]
pub struct Connect {
    pub uri: Uri,
    pub addr: Option<std::net::SocketAddr>,
}

/// An HTTP Client
///
/// ```rust
/// use ntex::http::client::Client;
///
/// #[ntex::main]
/// async fn main() {
///     let mut client = Client::default();
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
pub struct Client(Rc<ClientConfig>);

#[derive(Debug)]
struct ClientConfig {
    pub(self) connector: Box<dyn HttpConnect>,
    pub(self) headers: HeaderMap,
    pub(self) timeout: Millis,
}

impl Default for Client {
    fn default() -> Self {
        Client(Rc::new(ClientConfig {
            connector: Box::new(ConnectorWrapper(Connector::default().finish().into())),
            headers: HeaderMap::new(),
            timeout: Millis(5_000),
        }))
    }
}

impl Client {
    /// Create new client instance with default settings.
    pub fn new() -> Client {
        Client::default()
    }

    /// Build client instance.
    pub fn build() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// Construct HTTP request.
    pub fn request<U>(&self, method: Method, url: U) -> ClientRequest
    where
        Uri: TryFrom<U>,
        <Uri as TryFrom<U>>::Error: Into<HttpError>,
    {
        let mut req = ClientRequest::new(method, url, self.0.clone());

        for (key, value) in self.0.headers.iter() {
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
        for (key, value) in head.headers.iter() {
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
