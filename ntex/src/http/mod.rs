//! Http protocol support.
pub mod body;
mod builder;
pub mod client;
mod config;
#[cfg(feature = "compress")]
pub mod encoding;
pub(crate) mod helpers;
mod httpcodes;
mod httpmessage;
mod message;
mod payload;
mod request;
mod response;
mod service;

pub mod error;
pub mod h1;
pub mod h2;
pub mod header;
pub mod test;

pub(crate) use self::message::Message;

pub use self::builder::HttpServiceBuilder;
pub use self::client::Client;
pub use self::config::{DateService, KeepAlive, ServiceConfig};
pub use self::error::ResponseError;
pub use self::header::HeaderMap;
pub use self::httpmessage::HttpMessage;
pub use self::message::{ConnectionType, RequestHead, RequestHeadType, ResponseHead};
pub use self::payload::{Payload, PayloadStream};
pub use self::request::Request;
pub use self::response::{Response, ResponseBuilder};
pub use self::service::HttpService;

// re-exports
pub use http::uri::{self, Uri};
pub use http::{Method, StatusCode, Version};
pub use ntex_tls::types::HttpProtocol;
