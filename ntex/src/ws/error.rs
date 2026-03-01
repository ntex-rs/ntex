//! WebSocket protocol related errors.
use std::io;

use thiserror::Error;

use crate::error::Error;
use crate::http::error::{DecodeError, EncodeError, HttpError, ResponseError};
use crate::http::{Response, StatusCode, header::ALLOW, header::HeaderValue};
use crate::{connect::ConnectError, util::Either, util::clone_io_error};

use super::OpCode;

/// Websocket service errors
#[derive(Error, Debug)]
pub enum WsError<E> {
    #[error("Service error")]
    Service(E),
    /// Keep-alive error
    #[error("Keep-alive error")]
    KeepAlive,
    /// Frame read timeout
    #[error("Frame read timeout")]
    ReadTimeout,
    /// Ws protocol level error
    #[error("Ws protocol level error")]
    Protocol(ProtocolError),
    /// Peer has been disconnected
    #[error("Peer has been disconnected: {0:?}")]
    Disconnected(Option<io::Error>),
}

/// Websocket protocol errors
#[derive(Error, Copy, Clone, Debug)]
pub enum ProtocolError {
    /// Received an unmasked frame from client
    #[error("Received an unmasked frame from client")]
    UnmaskedFrame,
    /// Received a masked frame from server
    #[error("Received a masked frame from server")]
    MaskedFrame,
    /// Encountered invalid opcode
    #[error("Invalid opcode: {0}")]
    InvalidOpcode(u8),
    /// Invalid control frame length
    #[error("Invalid control frame length: {0}")]
    InvalidLength(usize),
    /// Bad web socket op code
    #[error("Bad web socket op code")]
    BadOpCode,
    /// A payload reached size limit.
    #[error("A payload reached size limit.")]
    Overflow,
    /// Continuation is not started
    #[error("Continuation is not started.")]
    ContinuationNotStarted,
    /// Received new continuation but it is already started
    #[error("Received new continuation but it is already started")]
    ContinuationStarted,
    /// Unknown continuation fragment
    #[error("Unknown continuation fragment {0}")]
    ContinuationFragment(OpCode),
}

/// Websocket client error
#[derive(Error, Clone, Debug)]
pub enum WsClientBuilderError<E> {
    #[error("Cannot create connector {0}")]
    Connector(E),
    #[error("Missing url scheme")]
    MissingScheme,
    #[error("Unknown url scheme")]
    UnknownScheme,
    #[error("Missing host name")]
    MissingHost,
    #[error("Url parse error: {0}")]
    Http(#[from] HttpError),
}

/// Websocket client error
#[derive(Error, Debug)]
pub enum WsClientError {
    /// Invalid request
    #[error("Invalid request")]
    InvalidRequest(#[from] EncodeError),
    /// Invalid response
    #[error("Invalid response")]
    InvalidResponse(#[from] DecodeError),
    /// Invalid response status
    #[error("Invalid response status")]
    InvalidResponseStatus(StatusCode),
    /// Invalid upgrade header
    #[error("Invalid upgrade header")]
    InvalidUpgradeHeader,
    /// Invalid connection header
    #[error("Invalid connection header")]
    InvalidConnectionHeader(HeaderValue),
    /// Missing CONNECTION header
    #[error("Missing CONNECTION header")]
    MissingConnectionHeader,
    /// Missing SEC-WEBSOCKET-ACCEPT header
    #[error("Missing SEC-WEBSOCKET-ACCEPT header")]
    MissingWebSocketAcceptHeader,
    /// Invalid challenge response
    #[error("Invalid challenge response")]
    InvalidChallengeResponse(String, HeaderValue),
    /// Protocol error
    #[error("{0}")]
    Protocol(#[from] ProtocolError),
    /// Response took too long
    #[error("Timeout out while waiting for response")]
    Timeout,
    /// Failed to connect to host
    #[error("Failed to connect to host: {0}")]
    Connect(#[from] Error<ConnectError>),
    /// Connector has been disconnected
    #[error("Connector has been disconnected: {0:?}")]
    Disconnected(Option<io::Error>),
}

impl From<Either<DecodeError, io::Error>> for WsClientError {
    fn from(err: Either<DecodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => WsClientError::InvalidResponse(err),
            Either::Right(err) => WsClientError::Disconnected(Some(err)),
        }
    }
}

impl From<Either<EncodeError, io::Error>> for WsClientError {
    fn from(err: Either<EncodeError, io::Error>) -> Self {
        match err {
            Either::Left(err) => WsClientError::InvalidRequest(err),
            Either::Right(err) => WsClientError::Disconnected(Some(err)),
        }
    }
}

impl Clone for WsClientError {
    fn clone(&self) -> Self {
        match self {
            WsClientError::InvalidRequest(err) => {
                WsClientError::InvalidRequest(err.clone())
            }
            WsClientError::InvalidResponse(err) => WsClientError::InvalidResponse(*err),
            WsClientError::InvalidResponseStatus(err) => {
                WsClientError::InvalidResponseStatus(*err)
            }
            WsClientError::InvalidUpgradeHeader => WsClientError::InvalidUpgradeHeader,
            WsClientError::InvalidConnectionHeader(err) => {
                WsClientError::InvalidConnectionHeader(err.clone())
            }
            WsClientError::MissingConnectionHeader => {
                WsClientError::MissingConnectionHeader
            }
            WsClientError::MissingWebSocketAcceptHeader => {
                WsClientError::MissingWebSocketAcceptHeader
            }
            WsClientError::InvalidChallengeResponse(n, val) => {
                WsClientError::InvalidChallengeResponse(n.clone(), val.clone())
            }
            WsClientError::Protocol(err) => WsClientError::Protocol(*err),
            WsClientError::Timeout => WsClientError::Timeout,
            WsClientError::Connect(err) => WsClientError::Connect(err.clone()),
            WsClientError::Disconnected(err) => {
                WsClientError::Disconnected(err.as_ref().map(clone_io_error))
            }
        }
    }
}

/// Websocket handshake errors
#[derive(Error, Copy, Clone, PartialEq, Eq, Debug)]
pub enum HandshakeError {
    /// Only get method is allowed
    #[error("Method not allowed")]
    GetMethodRequired,
    /// Upgrade header if not set to websocket
    #[error("Websocket upgrade is expected")]
    NoWebsocketUpgrade,
    /// Connection header is not set to upgrade
    #[error("Connection upgrade is expected")]
    NoConnectionUpgrade,
    /// Websocket version header is not set
    #[error("Websocket version header is required")]
    NoVersionHeader,
    /// Unsupported websocket version
    #[error("Unsupported version")]
    UnsupportedVersion,
    /// Websocket key is not set or wrong
    #[error("Unknown websocket key")]
    BadWebsocketKey,
}

impl ResponseError for HandshakeError {
    fn error_response(&self) -> Response {
        match *self {
            HandshakeError::GetMethodRequired => {
                Response::MethodNotAllowed().header(ALLOW, "GET").finish()
            }
            HandshakeError::NoWebsocketUpgrade => Response::BadRequest()
                .reason("No WebSocket UPGRADE header found")
                .finish(),
            HandshakeError::NoConnectionUpgrade => Response::BadRequest()
                .reason("No CONNECTION upgrade")
                .finish(),
            HandshakeError::NoVersionHeader => Response::BadRequest()
                .reason("Websocket version header is required")
                .finish(),
            HandshakeError::UnsupportedVersion => Response::BadRequest()
                .reason("Unsupported version")
                .finish(),
            HandshakeError::BadWebsocketKey => {
                Response::BadRequest().reason("Handshake error").finish()
            }
        }
    }
}

impl ResponseError for ProtocolError {}
