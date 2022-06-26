//! HTTP/2 implementation
mod payload;
mod service;

pub use self::payload::Payload;
pub use self::service::H2Service;

pub(in crate::http) use self::service::handle;
