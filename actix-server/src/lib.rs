//! General purpose tcp server

mod accept;
mod builder;
mod counter;
mod server;
mod service_config;
mod services;
mod signals;
pub mod ssl;
mod worker;

pub use actix_server_config::{Io, Protocol, ServerConfig};

pub use self::builder::ServerBuilder;
pub use self::server::Server;
pub use self::service_config::{ServiceConfig, ServiceRuntime};
pub use self::services::ServiceFactory;

#[doc(hidden)]
pub use self::services::ServiceFactory as StreamServiceFactory;

/// Socket id token
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Token(usize);

impl Token {
    pub(crate) fn next(&mut self) -> Token {
        let token = Token(self.0 + 1);
        self.0 += 1;
        token
    }
}

/// Start server building process
pub fn new() -> ServerBuilder {
    ServerBuilder::default()
}
