//! WebSocket protocol related errors.
use std::{error, io};

use derive_more::{Display, From};

use crate::connect::ConnectError;
use crate::http::error::{HttpError, ParseError};
use crate::http::{header::HeaderValue, StatusCode};
use crate::util::Either;

use super::OpCode;

/// Websocket service errors
#[derive(Debug, Display)]
pub enum WsError<E> {
    Service(E),
    /// Keep-alive error
    KeepAlive,
    /// Ws protocol level error
    Protocol(ProtocolError),
    /// Peer has been disconnected
    #[display(fmt = "Peer has been disconnected: {:?}", _0)]
    Disconnected(Option<io::Error>),
}

/// Websocket protocol errors
#[derive(Debug, Display, From)]
pub enum ProtocolError {
    /// Received an unmasked frame from client
    #[display(fmt = "Received an unmasked frame from client")]
    UnmaskedFrame,
    /// Received a masked frame from server
    #[display(fmt = "Received a masked frame from server")]
    MaskedFrame,
    /// Encountered invalid opcode
    #[display(fmt = "Invalid opcode: {}", _0)]
    InvalidOpcode(u8),
    /// Invalid control frame length
    #[display(fmt = "Invalid control frame length: {}", _0)]
    InvalidLength(usize),
    /// Bad web socket op code
    #[display(fmt = "Bad web socket op code")]
    BadOpCode,
    /// A payload reached size limit.
    #[display(fmt = "A payload reached size limit.")]
    Overflow,
    /// Continuation is not started
    #[display(fmt = "Continuation is not started.")]
    ContinuationNotStarted,
    /// Received new continuation but it is already started
    #[display(fmt = "Received new continuation but it is already started")]
    ContinuationStarted,
    /// Unknown continuation fragment
    #[display(fmt = "Unknown continuation fragment.")]
    ContinuationFragment(OpCode),
}

impl std::error::Error for ProtocolError {}

/// Websocket client error
#[derive(Debug, Display, From)]
pub enum WsClientBuilderError {
    #[display(fmt = "Missing url scheme")]
    MissingScheme,
    #[display(fmt = "Unknown url scheme")]
    UnknownScheme,
    #[display(fmt = "Missing host name")]
    MissingHost,
    #[display(fmt = "Url parse error: {}", _0)]
    Http(HttpError),
}

impl std::error::Error for WsClientBuilderError {}

/// Websocket client error
#[derive(Debug, Display, From)]
pub enum WsClientError {
    /// Invalid response
    #[display(fmt = "Invalid response")]
    InvalidResponse(ParseError),
    /// Invalid response status
    #[display(fmt = "Invalid response status")]
    InvalidResponseStatus(StatusCode),
    /// Invalid upgrade header
    #[display(fmt = "Invalid upgrade header")]
    InvalidUpgradeHeader,
    /// Invalid connection header
    #[display(fmt = "Invalid connection header")]
    InvalidConnectionHeader(HeaderValue),
    /// Missing CONNECTION header
    #[display(fmt = "Missing CONNECTION header")]
    MissingConnectionHeader,
    /// Missing SEC-WEBSOCKET-ACCEPT header
    #[display(fmt = "Missing SEC-WEBSOCKET-ACCEPT header")]
    MissingWebSocketAcceptHeader,
    /// Invalid challenge response
    #[display(fmt = "Invalid challenge response")]
    InvalidChallengeResponse(String, HeaderValue),
    /// Protocol error
    #[display(fmt = "{}", _0)]
    Protocol(ProtocolError),
    /// Response took too long
    #[display(fmt = "Timeout out while waiting for response")]
    Timeout,
    /// Failed to connect to host
    #[display(fmt = "Failed to connect to host: {}", _0)]
    Connect(ConnectError),
    /// Connector has been disconnected
    #[display(fmt = "Connector has been disconnected: {:?}", _0)]
    Disconnected(Option<io::Error>),
}

impl error::Error for WsClientError {}

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
