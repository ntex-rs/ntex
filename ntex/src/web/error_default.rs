//! Web error
use std::{error::Error, fmt, io, str::Utf8Error};

use serde::de::value::Error as DeError;
use serde_json::error::Error as JsonError;
use serde_urlencoded::ser::Error as FormError;

use crate::client;
use crate::http::{self, StatusCode, header};
use crate::util::timeout::TimeoutError;
#[cfg(feature = "ws")]
use crate::ws::error::HandshakeError;

use super::error::{InternalError, WebResponseError};
use super::{HttpRequest, HttpResponse, error};

/// Generic error container for errors that supports `DefaultError` renderer.
#[derive(thiserror::Error)]
pub struct WebError {
    cause: Box<dyn WebResponseError<Self>>,
}

impl WebError {
    pub fn new(err: impl WebResponseError<Self>) -> WebError {
        WebError {
            cause: Box::new(err),
        }
    }
}

impl<T> WebResponseError<T> for WebError {
    fn error_response(&mut self, req: &HttpRequest) -> HttpResponse {
        self.cause.error_response(req)
    }
}

impl crate::http::error::ResponseError for WebError {
    fn error_response(&self) -> HttpResponse {
        WebResponseError::<Self>::error_response_with_status(
            self,
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    }
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause, f)
    }
}

impl fmt::Debug for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "web::Error({:?})", self.cause)
    }
}

impl<T> WebResponseError<WebError> for InternalError<T>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        crate::http::error::ResponseError::error_response(self)
    }
}

/// `InternalServerError` for `StateExtractorError`
impl WebResponseError<WebError> for error::StateExtractorError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `InternalServerError` for `JsonError`
impl WebResponseError<WebError> for JsonError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `InternalServerError` for `FormError`
impl WebResponseError<WebError> for FormError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::Error`
impl WebResponseError<WebError> for tls_openssl::ssl::Error {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::HandshakeError`
impl<T: fmt::Debug + 'static> WebResponseError<WebError> for tls_openssl::ssl::HandshakeError<T> {}

/// Return `BAD_REQUEST` for `de::value::Error`
impl WebResponseError<WebError> for DeError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `InternalServerError` for `Canceled`
impl WebResponseError<WebError> for crate::http::error::Canceled {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// `InternalServerError` for `BlockingError`
impl<E: Error + 'static> WebResponseError<WebError> for crate::http::error::BlockingError<E> {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Return `BAD_REQUEST` for `Utf8Error`
impl WebResponseError<WebError> for Utf8Error {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// Return `InternalServerError` for `HttpError`,
/// Response generation can return `HttpError`, so it is internal error
impl WebResponseError<WebError> for crate::http::error::HttpError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Return `InternalServerError` for `io::Error`
impl WebResponseError<WebError> for io::Error {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        let status = match self.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        self.error_response_with_status(status)
    }
}

/// `InternalServerError` for `UrlGeneratorError`
impl WebResponseError<WebError> for error::UrlGenerationError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Response renderer for `UrlencodedError`
impl WebResponseError<WebError> for error::UrlencodedError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        let status = match *self {
            error::UrlencodedError::Overflow { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            error::UrlencodedError::UnknownLength => StatusCode::LENGTH_REQUIRED,
            _ => StatusCode::BAD_REQUEST,
        };
        self.error_response_with_status(status)
    }
}

/// Return `BadRequest` for `JsonPayloadError`
impl WebResponseError<WebError> for error::JsonPayloadError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        let status = match *self {
            error::JsonPayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        self.error_response_with_status(status)
    }
}

/// Error renderer for `PathError`
impl WebResponseError<WebError> for error::PathError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::NOT_FOUND)
    }
}

/// Error renderer `QueryPayloadError`
impl WebResponseError<WebError> for error::QueryPayloadError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

impl WebResponseError<WebError> for error::PayloadError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `PayloadError` returns two possible results:
///
/// - `Overflow` returns `PayloadTooLarge`
/// - Other errors returns `BadRequest`
impl WebResponseError<WebError> for http::error::PayloadError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        let status = match *self {
            http::error::PayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        self.error_response_with_status(status)
    }
}

#[cfg(feature = "cookie")]
/// Return `BadRequest` for `cookie::ParseError`
impl WebResponseError<WebError> for coo_kie::ParseError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// Return `BadRequest` for `ContentTypeError`
impl WebResponseError<WebError> for http::error::ContentTypeError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// Convert `ClientError` to a server `Response`
impl WebResponseError<WebError> for client::error::ClientError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
        let status = match self {
            client::error::ClientError::Connect(err) => {
                if matches!(err, client::error::ConnectError::Timeout) {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_REQUEST
                }
            }
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        self.error_response_with_status(status)
    }
}

#[cfg(feature = "ws")]
/// Error renderer for `ws::HandshakeError`
impl WebResponseError<WebError> for HandshakeError {
    fn error_response(&mut self, _: &HttpRequest) -> HttpResponse {
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

impl From<HandshakeError> for WebError {
    fn from(err: HandshakeError) -> Self {
        Self::new(err)
    }
}

/// Return `GATEWAY_TIMEOUT` for `TimeoutError`
impl<E> From<TimeoutError<E>> for WebError
where
    E: WebResponseError<WebError>,
{
    fn from(err: TimeoutError<E>) -> WebError {
        match err {
            TimeoutError::Service(e) => Self::new(e),
            TimeoutError::Timeout => Self::new(super::error::ErrorGatewayTimeout("")),
        }
    }
}
