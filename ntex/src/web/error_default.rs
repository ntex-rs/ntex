//! Web error
use std::{fmt, io, io::Write, str::Utf8Error};

use serde::de::value::Error as DeError;
use serde_json::error::Error as JsonError;
use serde_urlencoded::ser::Error as FormError;

use crate::client;
use crate::http::body::Body;
use crate::http::helpers::Writer;
use crate::http::{self, StatusCode, header};
use crate::util::{BytesMut, timeout::TimeoutError};
#[cfg(feature = "ws")]
use crate::ws::error::HandshakeError;

use super::error::{self, ErrorContainer, ErrorRenderer, WebResponseError};
use super::{HttpRequest, HttpResponse};

/// Default error type
#[derive(Clone, Copy, Default, Debug)]
pub struct DefaultError;

impl ErrorRenderer for DefaultError {
    type Container = Error;
}

/// Generic error container for errors that supports `DefaultError` renderer.
#[derive(thiserror::Error)]
pub struct Error {
    cause: Box<dyn WebResponseError<DefaultError>>,
}

impl Error {
    pub fn new<T: WebResponseError<DefaultError> + 'static>(err: T) -> Error {
        Error {
            cause: Box::new(err),
        }
    }

    /// Returns the reference to the underlying `WebResponseError`.
    pub fn as_response_error(&self) -> &dyn WebResponseError<DefaultError> {
        self.cause.as_ref()
    }
}

/// `Error` for any error which implements `WebResponseError<DefaultError>`
impl<T: WebResponseError<DefaultError>> From<T> for Error {
    fn from(err: T) -> Self {
        Error {
            cause: Box::new(err),
        }
    }
}

impl ErrorContainer for Error {
    fn error_response(&self, req: &HttpRequest) -> HttpResponse {
        self.cause.error_response(req)
    }
}

impl crate::http::error::ResponseError for Error {
    fn error_response(&self) -> HttpResponse {
        let mut resp = HttpResponse::new(self.cause.status_code());
        let mut buf = BytesMut::new();
        let _ = write!(Writer(&mut buf), "{}", self.cause);
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        resp.set_body(Body::from(buf))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause, f)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "web::Error({:?})", &self.cause)
    }
}

/// Return `GATEWAY_TIMEOUT` for `TimeoutError`
impl<E> From<TimeoutError<E>> for Error
where
    Error: From<E>,
{
    fn from(err: TimeoutError<E>) -> Error {
        match err {
            TimeoutError::Service(e) => e.into(),
            TimeoutError::Timeout => super::error::ErrorGatewayTimeout("").into(),
        }
    }
}

/// `InternalServerError` for `StateExtractorError`
impl WebResponseError<DefaultError> for error::StateExtractorError {}

/// `InternalServerError` for `JsonError`
impl WebResponseError<DefaultError> for JsonError {}

/// `InternalServerError` for `FormError`
impl WebResponseError<DefaultError> for FormError {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::Error`
impl WebResponseError<DefaultError> for tls_openssl::ssl::Error {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::HandshakeError`
impl<T: fmt::Debug + 'static> WebResponseError<DefaultError>
    for tls_openssl::ssl::HandshakeError<T>
{
}

/// Return `BAD_REQUEST` for `de::value::Error`
impl WebResponseError<DefaultError> for DeError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// `InternalServerError` for `Canceled`
impl WebResponseError<DefaultError> for crate::http::error::Canceled {}

/// `InternalServerError` for `BlockingError`
impl<E: fmt::Debug + 'static> WebResponseError<DefaultError>
    for crate::http::error::BlockingError<E>
{
}

/// Return `BAD_REQUEST` for `Utf8Error`
impl WebResponseError<DefaultError> for Utf8Error {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Return `InternalServerError` for `HttpError`,
/// Response generation can return `HttpError`, so it is internal error
impl WebResponseError<DefaultError> for crate::http::error::HttpError {}

/// Return `InternalServerError` for `io::Error`
impl WebResponseError<DefaultError> for io::Error {
    fn status_code(&self) -> StatusCode {
        match self.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// `InternalServerError` for `UrlGeneratorError`
impl WebResponseError<DefaultError> for error::UrlGenerationError {}

/// Response renderer for `UrlencodedError`
impl WebResponseError<DefaultError> for error::UrlencodedError {
    fn status_code(&self) -> StatusCode {
        match *self {
            error::UrlencodedError::Overflow { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            error::UrlencodedError::UnknownLength => StatusCode::LENGTH_REQUIRED,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// Return `BadRequest` for `JsonPayloadError`
impl WebResponseError<DefaultError> for error::JsonPayloadError {
    fn status_code(&self) -> StatusCode {
        match *self {
            error::JsonPayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// Error renderer for `PathError`
impl WebResponseError<DefaultError> for error::PathError {
    fn status_code(&self) -> StatusCode {
        StatusCode::NOT_FOUND
    }
}

/// Error renderer `QueryPayloadError`
impl WebResponseError<DefaultError> for error::QueryPayloadError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl WebResponseError<DefaultError> for error::PayloadError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// `PayloadError` returns two possible results:
///
/// - `Overflow` returns `PayloadTooLarge`
/// - Other errors returns `BadRequest`
impl WebResponseError<DefaultError> for http::error::PayloadError {
    fn status_code(&self) -> StatusCode {
        match *self {
            http::error::PayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

#[cfg(feature = "cookie")]
/// Return `BadRequest` for `cookie::ParseError`
impl WebResponseError<DefaultError> for coo_kie::ParseError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Return `BadRequest` for `ContentTypeError`
impl WebResponseError<DefaultError> for http::error::ContentTypeError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Convert `SendRequestError` to a server `Response`
impl WebResponseError<DefaultError> for client::error::SendRequestError {
    fn status_code(&self) -> StatusCode {
        match self {
            client::error::SendRequestError::Connect(err) => {
                if matches!(**err, client::error::ConnectError::Timeout) {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_REQUEST
                }
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[cfg(feature = "ws")]
/// Error renderer for `ws::HandshakeError`
impl WebResponseError<DefaultError> for HandshakeError {
    fn error_response(&self, _: &HttpRequest) -> HttpResponse {
        match *self {
            HandshakeError::GetMethodRequired => HttpResponse::MethodNotAllowed()
                .header(header::ALLOW, "GET")
                .finish(),
            HandshakeError::NoWebsocketUpgrade => HttpResponse::BadRequest()
                .reason("No WebSocket UPGRADE header found")
                .finish(),
            HandshakeError::NoConnectionUpgrade => HttpResponse::BadRequest()
                .reason("No CONNECTION upgrade")
                .finish(),
            HandshakeError::NoVersionHeader => HttpResponse::BadRequest()
                .reason("Websocket version header is required")
                .finish(),
            HandshakeError::UnsupportedVersion => HttpResponse::BadRequest()
                .reason("Unsupported version")
                .finish(),
            HandshakeError::BadWebsocketKey => HttpResponse::BadRequest()
                .reason("Handshake error")
                .finish(),
        }
    }
}
