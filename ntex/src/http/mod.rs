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
pub use ntex_http::{HeaderMap, Method, StatusCode, Version, body, header};

pub const ALPN_PROTO_H1: &[&str] = &["http/1.1"];
pub const ALPN_PROTO_H2: &[&str] = &["h2"];
pub const ALPN_PROTOS: &[&str] = &["h2", "http/1.1"];

/// Header item
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HeaderItem {
    pub name: header::HeaderName,
    pub origin: crate::util::ByteString,
    pub value: header::HeaderValue,
}

pub(crate) type HttpPipeline<St, B, Err> = crate::PipelineFactory<
    St,
    Request,
    Response<B>,
    Err,
    crate::SharedCfg,
    std::rc::Rc<dyn std::error::Error>,
>;

#[cfg(feature = "openssl")]
use crate::server::openssl::{SslAcceptor, SslFilter};
#[cfg(any(feature = "openssl", feature = "rustls"))]
use crate::{IntoService, Service, io::Filter, io::Io, io::Layer, server::TlsError};

#[cfg(feature = "openssl")]
/// Create openssl based service
pub fn openssl<F, S, St>(
    acceptor: tls_openssl::ssl::SslAcceptor,
    service: impl IntoService<S, St, Io<Layer<SslFilter, F>>>,
) -> impl Service<St, Io<F>, Res = S::Res, Error = TlsError<S::Error>>
where
    F: Filter,
    S: Service<St, Io<Layer<SslFilter, F>>>,
{
    SslAcceptor::new(acceptor)
        .map_err(TlsError::Tls)
        .and_then(service.into_service().map_err(TlsError::Service))
}

#[cfg(feature = "rustls")]
use crate::server::rustls::{TlsAcceptor, TlsServerFilter};

#[cfg(feature = "rustls")]
/// Create rustls based service.
///
/// You must specify alpns protocols to negotiate for h2 server
pub fn rustls<F, S, St>(
    mut config: tls_rustls::ServerConfig,
    protos: &[&str],
    service: impl IntoService<S, St, Io<Layer<TlsServerFilter, F>>>,
) -> impl Service<St, Io<F>, Res = S::Res, Error = TlsError<S::Error>>
where
    F: Filter,
    S: Service<St, Io<Layer<TlsServerFilter, F>>>,
{
    if !protos.is_empty() && config.alpn_protocols.is_empty() {
        config.alpn_protocols = protos.iter().map(|s| s.to_string().into()).collect();
    }

    TlsAcceptor::new(std::sync::Arc::new(config))
        .map_err(TlsError::Tls)
        .and_then(service.into_service().map_err(TlsError::Service))
}
