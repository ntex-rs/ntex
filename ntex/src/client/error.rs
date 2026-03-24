//! Http client errors
use std::{error::Error, io, ops::Deref, rc::Rc};

use serde_json::error::Error as JsonError;

#[cfg(feature = "openssl")]
use tls_openssl::ssl::{Error as SslError, HandshakeError};

use crate::error::{ErrorDiagnostic, ResultType};
use crate::http::error::{DecodeError, EncodeError, HttpError, PayloadError};
use crate::util::{Either, clone_io_error};

/// A set of errors that can occur during parsing json payloads
#[derive(thiserror::Error, Debug)]
pub enum JsonPayloadError {
    /// Content type error
    #[error("Content type error")]
    ContentType,
    /// Deserialize error
    #[error("Json deserialize error")]
    Deserialize(#[source] Option<JsonError>),
    /// Payload error
    #[error("Error that occur during reading payload: {0}")]
    Payload(#[from] ClientPayloadError),
}

impl Clone for JsonPayloadError {
    fn clone(&self) -> Self {
        match self {
            JsonPayloadError::ContentType => JsonPayloadError::ContentType,
            JsonPayloadError::Deserialize(_) => JsonPayloadError::Deserialize(None),
            JsonPayloadError::Payload(err) => JsonPayloadError::Payload(err.clone()),
        }
    }
}

impl From<JsonError> for JsonPayloadError {
    fn from(err: JsonError) -> JsonPayloadError {
        JsonPayloadError::Deserialize(Some(err))
    }
}

impl From<PayloadError> for JsonPayloadError {
    fn from(err: PayloadError) -> JsonPayloadError {
        JsonPayloadError::Payload(ClientPayloadError(err))
    }
}

impl ErrorDiagnostic for JsonPayloadError {
    type Kind = ResultType;

    fn kind(&self) -> ResultType {
        ResultType::ServiceError
    }
}

#[derive(thiserror::Error, Clone, Debug)]
#[error("{0}")]
pub struct ClientPayloadError(#[from] pub(crate) PayloadError);

impl Deref for ClientPayloadError {
    type Target = PayloadError;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ErrorDiagnostic for ClientPayloadError {
    type Kind = ResultType;

    fn kind(&self) -> ResultType {
        ResultType::ServiceError
    }
}

/// A set of errors that can occur while building HTTP client
#[derive(thiserror::Error, Copy, Clone, Debug)]
pub enum ClientBuilderError {
    /// Connector failed
    #[error("Cannot construct connector")]
    ConnectorFailed,
}

/// A set of errors that can occur while connecting to an HTTP host
#[derive(thiserror::Error, Debug)]
pub enum ConnectError {
    /// SSL feature is not enabled
    #[error("SSL is not supported")]
    SslIsNotSupported,

    /// SSL error
    #[cfg(feature = "openssl")]
    #[error("{0}")]
    SslError(#[source] Rc<SslError>),

    /// SSL Handshake error
    #[cfg(feature = "openssl")]
    #[error("{0}")]
    SslHandshakeError(String),

    /// Failed to resolve the hostname
    #[error("Failed resolving hostname: {0}")]
    Resolver(
        #[from]
        #[source]
        io::Error,
    ),

    /// No dns records
    #[error("No dns records found for the input")]
    NoRecords,

    /// Connecting took too long
    #[error("Timeout while establishing connection")]
    Timeout,

    /// Connector has been disconnected
    #[error("Connector has been disconnected")]
    Disconnected(#[source] Option<io::Error>),

    /// Unresolved host name
    #[error("Connector received `Connect` method with unresolved host")]
    Unresolved,
}

impl ErrorDiagnostic for ConnectError {
    type Kind = ResultType;

    fn kind(&self) -> ResultType {
        ResultType::ServiceError
    }
}

impl Clone for ConnectError {
    fn clone(&self) -> Self {
        match self {
            ConnectError::SslIsNotSupported => ConnectError::SslIsNotSupported,
            #[cfg(feature = "openssl")]
            ConnectError::SslError(e) => ConnectError::SslError(e.clone()),
            #[cfg(feature = "openssl")]
            ConnectError::SslHandshakeError(e) => {
                ConnectError::SslHandshakeError(e.clone())
            }
            ConnectError::Resolver(e) => ConnectError::Resolver(clone_io_error(e)),
            ConnectError::NoRecords => ConnectError::NoRecords,
            ConnectError::Timeout => ConnectError::Timeout,
            ConnectError::Disconnected(e) => {
                if let Some(e) = e {
                    ConnectError::Disconnected(Some(clone_io_error(e)))
                } else {
                    ConnectError::Disconnected(None)
                }
            }
            ConnectError::Unresolved => ConnectError::Unresolved,
        }
    }
}

#[cfg(feature = "openssl")]
impl From<SslError> for ConnectError {
    fn from(err: SslError) -> Self {
        ConnectError::SslError(Rc::new(err))
    }
}

impl From<crate::connect::ConnectError> for ConnectError {
    fn from(err: crate::connect::ConnectError) -> ConnectError {
        match err {
            crate::connect::ConnectError::Resolver(e) => ConnectError::Resolver(e),
            crate::connect::ConnectError::NoRecords => ConnectError::NoRecords,
            crate::connect::ConnectError::InvalidInput => panic!(),
            crate::connect::ConnectError::Unresolved => ConnectError::Unresolved,
            crate::connect::ConnectError::Io(e) => ConnectError::Disconnected(Some(e)),
        }
    }
}

#[cfg(feature = "openssl")]
impl<T: std::fmt::Debug> From<HandshakeError<T>> for ConnectError {
    fn from(err: HandshakeError<T>) -> ConnectError {
        ConnectError::SslHandshakeError(format!("{err:?}"))
    }
}

#[derive(Copy, Clone, Debug, thiserror::Error)]
pub enum InvalidUrl {
    #[error("Missing url scheme")]
    MissingScheme,
    #[error("Unknown url scheme")]
    UnknownScheme,
    #[error("Missing host name")]
    MissingHost,
    #[error("Url parse error: {0}")]
    Http(#[from] HttpError),
}

#[doc(hidden)]
#[deprecated(since = "3.2.0", note = "ClientError")]
pub type SendRequestError = ClientError;

/// A set of errors that can occur during request sending and response reading
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// Invalid URL
    #[error("Invalid URL: {0}")]
    Url(
        #[from]
        #[source]
        InvalidUrl,
    ),
    /// Failed to connect to host
    #[error("Failed to connect to host: {0}")]
    Connect(
        #[from]
        #[source]
        ConnectError,
    ),
    /// Error sending request
    #[error("Error sending request: {0}")]
    Send(
        #[from]
        #[source]
        io::Error,
    ),
    /// Error encoding request
    #[error("Error during request encoding: {0}")]
    Request(
        #[from]
        #[source]
        EncodeError,
    ),
    /// Error parsing response
    #[error("Error during response parsing: {0}")]
    Response(
        #[from]
        #[source]
        DecodeError,
    ),
    /// Http error
    #[error("{0}")]
    Http(
        #[from]
        #[source]
        HttpError,
    ),
    /// Http2 error
    #[error("Http2 error {0}")]
    H2(
        #[from]
        #[source]
        ntex_h2::OperationError,
    ),
    /// Response took too long
    #[error("Timeout while waiting for response")]
    Timeout,
    /// Tunnels are not supported for http2 connection
    #[error("Tunnels are not supported for http2 connection")]
    TunnelNotSupported,
    /// Error sending request body
    #[error("Error sending request body {0}")]
    Error(
        #[from]
        #[source]
        Rc<dyn Error>,
    ),
}

impl Clone for ClientError {
    fn clone(&self) -> ClientError {
        match self {
            ClientError::Url(err) => ClientError::Url(err.clone()),
            ClientError::Connect(err) => ClientError::Connect(err.clone()),
            ClientError::Request(err) => ClientError::Request(err.clone()),
            ClientError::Response(err) => ClientError::Response(*err),
            ClientError::Http(err) => ClientError::Http(*err),
            ClientError::H2(err) => ClientError::H2(err.clone()),
            ClientError::Timeout => ClientError::Timeout,
            ClientError::TunnelNotSupported => ClientError::TunnelNotSupported,
            ClientError::Error(err) => ClientError::Error(err.clone()),
            ClientError::Send(err) => ClientError::Send(crate::util::clone_io_error(err)),
        }
    }
}

impl From<Either<EncodeError, io::Error>> for ClientError {
    fn from(err: Either<EncodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => ClientError::Request(err),
            Either::Right(err) => ClientError::Send(err),
        }
    }
}

impl From<Either<DecodeError, io::Error>> for ClientError {
    fn from(err: Either<DecodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => ClientError::Response(err),
            Either::Right(err) => ClientError::Send(err),
        }
    }
}

impl ErrorDiagnostic for ClientError {
    type Kind = ResultType;

    fn kind(&self) -> ResultType {
        match self {
            ClientError::Url(_) | ClientError::Http(_) => ResultType::ClientError,
            ClientError::Connect(err) => err.kind(),
            ClientError::Send(_)
            | ClientError::Request(_)
            | ClientError::Response(_)
            | ClientError::H2(_)
            | ClientError::Timeout
            | ClientError::TunnelNotSupported
            | ClientError::Error(_) => ResultType::ServiceError,
        }
    }
}

impl From<ClientBuilderError> for io::Error {
    fn from(err: ClientBuilderError) -> io::Error {
        io::Error::other(err)
    }
}
