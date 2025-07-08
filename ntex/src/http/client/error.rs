//! Http client errors
use std::{error::Error, io, rc::Rc};

use serde_json::error::Error as JsonError;
use thiserror::Error;

#[cfg(feature = "openssl")]
use tls_openssl::ssl::{Error as SslError, HandshakeError};

use crate::http::error::{DecodeError, EncodeError, HttpError, PayloadError};
use crate::util::{clone_io_error, Either};

/// A set of errors that can occur during parsing json payloads
#[derive(Error, Debug)]
pub enum JsonPayloadError {
    /// Content type error
    #[error("Content type error")]
    ContentType,
    /// Deserialize error
    #[error("Json deserialize error: {0}")]
    Deserialize(#[from] JsonError),
    /// Payload error
    #[error("Error that occur during reading payload: {0}")]
    Payload(#[from] PayloadError),
}

/// A set of errors that can occur while connecting to an HTTP host
#[derive(Error, Debug)]
pub enum ConnectError {
    /// SSL feature is not enabled
    #[error("SSL is not supported")]
    SslIsNotSupported,

    /// SSL error
    #[cfg(feature = "openssl")]
    #[error("{0}")]
    SslError(Rc<SslError>),

    /// SSL Handshake error
    #[cfg(feature = "openssl")]
    #[error("{0}")]
    SslHandshakeError(String),

    /// Failed to resolve the hostname
    #[error("Failed resolving hostname: {0}")]
    Resolver(#[from] io::Error),

    /// No dns records
    #[error("No dns records found for the input")]
    NoRecords,

    /// Connecting took too long
    #[error("Timeout while establishing connection")]
    Timeout,

    /// Connector has been disconnected
    #[error("Connector has been disconnected")]
    Disconnected(Option<io::Error>),

    /// Unresolved host name
    #[error("Connector received `Connect` method with unresolved host")]
    Unresolved,
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
        ConnectError::SslError(std::rc::Rc::new(err))
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

#[derive(Clone, Error, Debug)]
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

/// A set of errors that can occur during request sending and response reading
#[derive(Error, Debug)]
pub enum SendRequestError {
    /// Invalid URL
    #[error("Invalid URL: {0}")]
    Url(#[from] InvalidUrl),
    /// Failed to connect to host
    #[error("Failed to connect to host: {0}")]
    Connect(#[from] ConnectError),
    /// Error sending request
    #[error("Error sending request: {0}")]
    Send(#[from] io::Error),
    /// Error encoding request
    #[error("Error during request encoding: {0}")]
    Request(#[from] EncodeError),
    /// Error parsing response
    #[error("Error during response parsing: {0}")]
    Response(#[from] DecodeError),
    /// Http error
    #[error("{0}")]
    Http(#[from] HttpError),
    /// Http2 error
    #[error("Http2 error {0}")]
    H2(#[from] ntex_h2::OperationError),
    /// Response took too long
    #[error("Timeout while waiting for response")]
    Timeout,
    /// Tunnels are not supported for http2 connection
    #[error("Tunnels are not supported for http2 connection")]
    TunnelNotSupported,
    /// Error sending request body
    #[error("Error sending request body {0}")]
    Error(#[from] Rc<dyn Error>),
}

impl Clone for SendRequestError {
    fn clone(&self) -> SendRequestError {
        match self {
            SendRequestError::Url(err) => SendRequestError::Url(err.clone()),
            SendRequestError::Connect(err) => SendRequestError::Connect(err.clone()),
            SendRequestError::Request(err) => SendRequestError::Request(err.clone()),
            SendRequestError::Response(err) => SendRequestError::Response(*err),
            SendRequestError::Http(err) => SendRequestError::Http(err.clone()),
            SendRequestError::H2(err) => SendRequestError::H2(err.clone()),
            SendRequestError::Timeout => SendRequestError::Timeout,
            SendRequestError::TunnelNotSupported => SendRequestError::TunnelNotSupported,
            SendRequestError::Error(err) => SendRequestError::Error(err.clone()),
            SendRequestError::Send(err) => {
                SendRequestError::Send(crate::util::clone_io_error(err))
            }
        }
    }
}

impl From<Either<EncodeError, io::Error>> for SendRequestError {
    fn from(err: Either<EncodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => SendRequestError::Request(err),
            Either::Right(err) => SendRequestError::Send(err),
        }
    }
}

impl From<Either<DecodeError, io::Error>> for SendRequestError {
    fn from(err: Either<DecodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => SendRequestError::Response(err),
            Either::Right(err) => SendRequestError::Send(err),
        }
    }
}

/// A set of errors that can occur during freezing a request
#[derive(Error, Debug, Clone)]
pub enum FreezeRequestError {
    /// Invalid URL
    #[error("Invalid URL: {0}")]
    Url(#[from] InvalidUrl),
    /// Http error
    #[error("{0}")]
    Http(#[from] HttpError),
}

impl From<FreezeRequestError> for SendRequestError {
    fn from(e: FreezeRequestError) -> Self {
        match e {
            FreezeRequestError::Url(e) => e.into(),
            FreezeRequestError::Http(e) => e.into(),
        }
    }
}
