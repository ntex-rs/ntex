//! HTTP/2 implementation
mod default;
pub(super) mod payload;
mod service;

pub use ntex_h2::{Control, ControlAck, ServiceConfig as Http2ServiceConfig, control};

pub use self::default::DefaultControlService;
pub use self::payload::Payload;
pub use self::service::H2Service;

pub(in crate::http) use self::service::handle;
