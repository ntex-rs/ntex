//! WebSocket protocol related errors.
use std::io;

use crate::error::ErrorDiagnostic;
use crate::http::error::{DecodeError, EncodeError, HttpError, ResponseError};
use crate::http::{Response, StatusCode, header::ALLOW, header::HeaderValue};
use crate::{connect::ConnectError, util::Either, util::clone_io_error};

use super::OpCode;

/// Errors produced by a WebSocket dispatcher.
#[derive(Debug, thiserror::Error)]
pub enum WsError<E> {
    /// Error returned by the frame service.
    #[error("Service error")]
    Service(#[source] E),
    /// The keep-alive timer expired.
    #[error("Keep-alive error")]
    KeepAlive,
    /// Reading a frame timed out.
    #[error("Frame read timeout")]
    ReadTimeout,
    /// Write backpressure stayed enabled for longer than the write timeout.
    #[error("Write timeout")]
    WriteTimeout,
    /// WebSocket protocol error.
    #[error("Ws protocol level error")]
    Protocol(#[source] ProtocolError),
    /// WebSocket opening-handshake error.
    #[error("Ws handshake error")]
    Handshake(#[from] HandshakeError),
    /// The peer disconnected.
    #[error("Peer has been disconnected: {0:?}")]
    Disconnected(#[source] Option<io::Error>),
}

/// WebSocket protocol errors.
#[derive(Copy, Clone, Debug, thiserror::Error)]
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
    /// Reserved frame bits are set without a negotiated extension.
    #[error("Reserved frame bits are set: {0:#05b}")]
    ReservedBits(u8),
    /// A control frame is fragmented.
    #[error("Fragmented control frame: {0}")]
    FragmentedControlFrame(OpCode),
    /// Invalid control frame length
    #[error("Invalid control frame length: {0}")]
    InvalidLength(usize),
    /// A payload length does not use its shortest valid encoding.
    #[error("Invalid payload length encoding")]
    InvalidLengthEncoding,
    /// Invalid close status code.
    #[error("Invalid close status code: {0}")]
    InvalidCloseCode(u16),
    /// Invalid close-frame payload.
    #[error("Invalid close-frame payload")]
    InvalidClosePayload,
    /// A close-frame description is not valid UTF-8.
    #[error("Invalid UTF-8 in close-frame description")]
    InvalidUtf8,
    /// A message was encoded after a close message.
    #[error("WebSocket codec is closed")]
    Closed,
    /// A payload reached size limit.
    #[error("A payload reached size limit.")]
    Overflow,
    /// Continuation is not started
    #[error("Continuation is not started.")]
    ContinuationNotStarted,
    /// Received new continuation but it is already started
    #[error("Received new continuation but it is already started")]
    ContinuationStarted,
}

/// Errors produced while configuring a WebSocket client.
#[derive(Clone, Debug, thiserror::Error)]
pub enum WsConfigError {
    /// The URI does not contain a scheme.
    #[error("Missing url scheme")]
    MissingScheme,
    /// The URI uses an unsupported scheme.
    #[error("Unknown url scheme")]
    UnknownScheme,
    /// The URI does not contain a host.
    #[error("Missing host name")]
    MissingHost,
    /// The URI could not be parsed.
    #[error("Url parse error: {0}")]
    Http(
        #[from]
        #[source]
        HttpError,
    ),
}

/// Errors produced while establishing or using a WebSocket client connection.
#[derive(Debug, thiserror::Error)]
pub enum WsClientError {
    /// Invalid client configuration.
    #[error("Invalid client configuration: {0}")]
    Config(
        #[from]
        #[source]
        WsConfigError,
    ),
    /// Invalid request
    #[error("Invalid request")]
    InvalidRequest(
        #[from]
        #[source]
        EncodeError,
    ),
    /// Invalid response
    #[error("Invalid response")]
    InvalidResponse(
        #[from]
        #[source]
        DecodeError,
    ),
    /// Invalid response status
    #[error("Invalid response status: {0}")]
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
    /// The server selected an invalid or unrequested WebSocket subprotocol.
    #[error("Invalid WebSocket subprotocol: {0:?}")]
    InvalidWebSocketProtocol(HeaderValue),
    /// The server returned an extension that the client did not offer.
    #[error("Unexpected WebSocket extensions: {0:?}")]
    UnexpectedWebSocketExtensions(HeaderValue),
    /// Protocol error
    #[error("{0}")]
    Protocol(
        #[from]
        #[source]
        ProtocolError,
    ),
    /// The opening handshake timed out.
    #[error("Timeout while waiting for response")]
    Timeout,
    /// Failed to connect to host
    #[error("Failed to connect to host: {0}")]
    Connect(
        #[from]
        #[source]
        ConnectError,
    ),
    /// Connector has been disconnected
    #[error("Connector has been disconnected: {0:?}")]
    Disconnected(#[source] Option<io::Error>),
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
            WsClientError::Config(err) => WsClientError::Config(err.clone()),
            WsClientError::InvalidRequest(err) => WsClientError::InvalidRequest(err.clone()),
            WsClientError::InvalidResponse(err) => WsClientError::InvalidResponse(*err),
            WsClientError::InvalidResponseStatus(err) => WsClientError::InvalidResponseStatus(*err),
            WsClientError::InvalidUpgradeHeader => WsClientError::InvalidUpgradeHeader,
            WsClientError::InvalidConnectionHeader(err) => {
                WsClientError::InvalidConnectionHeader(err.clone())
            }
            WsClientError::MissingConnectionHeader => WsClientError::MissingConnectionHeader,
            WsClientError::MissingWebSocketAcceptHeader => {
                WsClientError::MissingWebSocketAcceptHeader
            }
            WsClientError::InvalidChallengeResponse(n, val) => {
                WsClientError::InvalidChallengeResponse(n.clone(), val.clone())
            }
            WsClientError::InvalidWebSocketProtocol(val) => {
                WsClientError::InvalidWebSocketProtocol(val.clone())
            }
            WsClientError::UnexpectedWebSocketExtensions(val) => {
                WsClientError::UnexpectedWebSocketExtensions(val.clone())
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

impl ErrorDiagnostic for WsClientError {
    fn signature(&self) -> &'static str {
        "ntex-ws-client"
    }
}

/// Errors produced while validating a WebSocket opening handshake.
#[derive(Copy, Clone, PartialEq, Eq, Debug, thiserror::Error)]
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
    /// The selected WebSocket subprotocol was not requested by the client.
    #[error("Invalid websocket subprotocol")]
    BadWebsocketProtocol,
}

impl ResponseError for HandshakeError {
    fn error_response(&self) -> Response {
        match *self {
            HandshakeError::GetMethodRequired => {
                Response::MethodNotAllowed().header(ALLOW, "GET").build()
            }
            HandshakeError::NoWebsocketUpgrade => Response::BadRequest()
                .reason("No WebSocket UPGRADE header found")
                .build(),
            HandshakeError::NoConnectionUpgrade => Response::BadRequest()
                .reason("No CONNECTION upgrade")
                .build(),
            HandshakeError::NoVersionHeader => Response::BadRequest()
                .reason("Websocket version header is required")
                .build(),
            HandshakeError::UnsupportedVersion => {
                Response::BadRequest().reason("Unsupported version").build()
            }
            HandshakeError::BadWebsocketKey => {
                Response::BadRequest().reason("Handshake error").build()
            }
            HandshakeError::BadWebsocketProtocol => Response::BadRequest()
                .reason("Invalid websocket subprotocol")
                .build(),
        }
    }
}

impl ResponseError for ProtocolError {}
