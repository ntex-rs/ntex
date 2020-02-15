//! Http client api
use http::Uri;

mod connection;
mod connector;
pub mod error;
mod h1proto;
mod h2proto;
mod pool;

pub use self::connection::Connection;
pub use self::connector::Connector;

#[derive(Clone)]
pub struct Connect {
    pub uri: Uri,
    pub addr: Option<std::net::SocketAddr>,
}
