//! HTTP protocol support.
//!
//! This module contains HTTP request and response types, HTTP/1 and HTTP/2
//! services, payload streaming, protocol configuration, and common header
//! types.
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

pub(crate) use self::message::{CurrentIo, IoAccess, Message};

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

/// ALPN protocol identifiers for HTTP/1.1.
pub const ALPN_PROTO_H1: &[&str] = &["http/1.1"];
/// ALPN protocol identifiers for HTTP/2.
pub const ALPN_PROTO_H2: &[&str] = &["h2"];
/// ALPN protocol identifiers for negotiating HTTP/2 or HTTP/1.1.
pub const ALPN_PROTOS: &[&str] = &["h2", "http/1.1"];

/// A parsed header that preserves its original name and value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HeaderItem {
    /// Parsed header name.
    pub name: header::HeaderName,
    /// Header name as it appeared in the original message.
    pub origin: crate::util::ByteString,
    /// Parsed header value.
    pub value: header::HeaderValue,
}

#[cfg(feature = "openssl")]
use crate::server::openssl::{SslAcceptor, SslFilter};
#[cfg(any(feature = "openssl", feature = "rustls"))]
use crate::{IntoService, Service, io::Filter, io::Io, io::Layer, server::TlsError};

#[cfg(feature = "openssl")]
/// Wraps an HTTP service in an OpenSSL TLS acceptor.
///
/// ALPN behavior comes from the supplied `acceptor`; configure it with
/// [`ALPN_PROTO_H1`], [`ALPN_PROTO_H2`], or [`ALPN_PROTOS`] as appropriate.
/// TLS failures and inner-service failures are mapped to the corresponding
/// [`TlsError`] variants.
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
/// Creates a rustls-based HTTP service.
///
/// Pass the supported ALPN protocol identifiers in `protos` to enable HTTP/2
/// negotiation. If the configuration already contains ALPN protocols, they
/// are preserved. TLS failures and inner-service failures are mapped to the
/// corresponding [`TlsError`] variants.
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

use crate::error::Error;
use crate::service::pipeline::PipelineFactory;

type HttpPipeline<St, Err> = PipelineFactory<St, Request, Response, Err, error::DispatchError>;
type Ctl1Pipeline<St, F, Err> = PipelineFactory<
    St,
    h1::Control<F, Err>,
    h1::ControlAck<F>,
    error::DispatchError,
    error::DispatchError,
>;
type Ctl2Pipeline<St> = PipelineFactory<
    St,
    h2::Control<Error<error::H2Error>>,
    h2::ControlAck,
    error::DispatchError,
    error::DispatchError,
>;
