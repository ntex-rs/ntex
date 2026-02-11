//! Http protocol support.
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

pub use self::config::{DateService, HttpServiceConfig, KeepAlive};
pub use self::error::ResponseError;
pub use self::httpmessage::HttpMessage;
pub use self::message::{ConnectionType, RequestHead, ResponseHead};
pub use self::payload::{Payload, PayloadStream};
pub use self::request::Request;
pub use self::response::{Response, ResponseBuilder};
pub use self::service::HttpService;
pub use crate::io::types::HttpProtocol;

// re-exports
pub use ntex_http::uri::{self, Uri};
pub use ntex_http::{HeaderMap, Method, StatusCode, Version, body};
