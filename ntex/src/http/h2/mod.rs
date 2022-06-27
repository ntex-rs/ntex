//! HTTP/2 implementation
pub(super) mod payload;
mod service;

pub use self::payload::Payload;
pub use self::service::H2Service;

pub(in crate::http) use self::service::handle;
