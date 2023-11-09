//! WebSocket protocol related errors.
use std::io;

use thiserror::Error;

use crate::http::error::{HttpError, ParseError, ResponseError};
use crate::http::{header::HeaderValue, header::ALLOW, Response, StatusCode};
use crate::{connect::ConnectError, util::Either};

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
#[derive(Error, Debug)]
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
#[derive(Error, Debug)]
pub enum WsClientBuilderError {
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
    /// Invalid response
    #[error("Invalid response")]
    InvalidResponse(#[from] ParseError),
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
    Connect(#[from] ConnectError),
    /// Connector has been disconnected
    #[error("Connector has been disconnected: {0:?}")]
    Disconnected(Option<io::Error>),
}

impl From<Either<ParseError, io::Error>> for WsClientError {
    fn from(err: Either<ParseError, io::Error>) -> Self {
        match err {
            Either::Left(err) => WsClientError::InvalidResponse(err),
            Either::Right(err) => WsClientError::Disconnected(Some(err)),
        }
    }
}

impl From<Either<io::Error, io::Error>> for WsClientError {
    fn from(err: Either<io::Error, io::Error>) -> Self {
        WsClientError::Disconnected(Some(err.into_inner()))
    }
}

/// Websocket handshake errors
#[derive(Error, PartialEq, Eq, Debug)]
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
