//! Basic http primitives for actix-net framework.
pub mod body;
mod builder;
pub mod client;
mod cloneable;
mod config;
#[cfg(feature = "compress")]
pub mod encoding;
mod extensions;
mod helpers;
mod httpcodes;
mod httpmessage;
mod message;
mod payload;
mod request;
mod response;
mod service;
mod time_parser;

pub mod cookie;
pub mod error;
pub mod h1;
pub mod h2;
pub mod header;
pub mod test;
pub mod ws;

pub use self::builder::HttpServiceBuilder;
pub use self::config::{KeepAlive, ServiceConfig};
pub use self::error::{Error, ResponseError};
pub use self::extensions::Extensions;
pub use self::header::HeaderMap;
pub use self::httpmessage::HttpMessage;
pub use self::message::{
    ConnectionType, Message, RequestHead, RequestHeadType, ResponseHead,
};
pub use self::payload::{Payload, PayloadStream};
pub use self::request::Request;
pub use self::response::{Response, ResponseBuilder};
pub use self::service::HttpService;

// re-exports
pub use http::uri::{self, Uri};
pub use http::{Method, StatusCode, Version};

/// Http protocol
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Http1,
    Http2,
}
