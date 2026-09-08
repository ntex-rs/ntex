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

/// Generic error container for errors.
#[derive(thiserror::Error)]
pub struct DefaultError {
    cause: Box<dyn FnOnce(&HttpRequest) -> HttpResponse>, //dyn WebResponseError<DefaultError>>,
}

impl DefaultError {
    pub fn new<Err>(err: impl WebResponseError<Err>) -> Self {
        Self {
            cause: Box::new(|req| err.error_response(req)),
        }
    }

    pub fn error_response(self, req: &HttpRequest) -> HttpResponse {
        (self.cause)(req)
    }
}

impl<E: WebResponseError<DefaultError>> From<E> for DefaultError {
    fn from(e: E) -> Self {
        Self::new(e)
    }
}

impl crate::http::error::ResponseError for DefaultError {
    fn error_response(&self) -> HttpResponse {
        HttpResponse::new(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl fmt::Display for DefaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "web::DefaultError")
    }
}

impl fmt::Debug for DefaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "web::DefaultError")
    }
}

// ======================= WebError =========================

/// Generic http error.
#[derive(thiserror::Error)]
pub struct WebError {
    response: HttpResponse,
}

impl WebError {
    pub fn new(response: HttpResponse) -> WebError {
        WebError { response }
    }
}

impl<Err> WebResponseError<Err> for WebError {
    fn error_response(self, req: &HttpRequest) -> HttpResponse {
        self.response
    }
}

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "web::Error(status: {:?})", self.response.status())
    }
}

impl fmt::Debug for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "web::Error(status: {:?})", self.response.status())
    }
}

// =========== DefaultError impls

impl<T> WebResponseError<DefaultError> for InternalError<T>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        crate::http::error::ResponseError::error_response(&self)
    }
}

/// `InternalServerError` for `StateExtractorError`
impl WebResponseError<DefaultError> for error::StateExtractorError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `InternalServerError` for `JsonError`
impl WebResponseError<DefaultError> for JsonError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `InternalServerError` for `FormError`
impl WebResponseError<DefaultError> for FormError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::Error`
impl WebResponseError<DefaultError> for tls_openssl::ssl::Error {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::HandshakeError`
impl<T: fmt::Debug + 'static> WebResponseError<DefaultError>
    for tls_openssl::ssl::HandshakeError<T>
{
}

/// Return `BAD_REQUEST` for `de::value::Error`
impl WebResponseError<DefaultError> for DeError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `InternalServerError` for `Canceled`
impl WebResponseError<DefaultError> for crate::http::error::Canceled {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// `InternalServerError` for `BlockingError`
impl<E: Error + 'static> WebResponseError<DefaultError> for crate::http::error::BlockingError<E> {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Return `BAD_REQUEST` for `Utf8Error`
impl WebResponseError<DefaultError> for Utf8Error {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// Return `InternalServerError` for `HttpError`,
/// Response generation can return `HttpError`, so it is internal error
impl WebResponseError<DefaultError> for crate::http::error::HttpError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Return `InternalServerError` for `io::Error`
impl WebResponseError<DefaultError> for io::Error {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        let status = match self.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        self.error_response_with_status(status)
    }
}

/// `InternalServerError` for `UrlGeneratorError`
impl WebResponseError<DefaultError> for error::UrlGenerationError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Response renderer for `UrlencodedError`
impl WebResponseError<DefaultError> for error::UrlencodedError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        let status = match self {
            error::UrlencodedError::Overflow { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            error::UrlencodedError::UnknownLength => StatusCode::LENGTH_REQUIRED,
            _ => StatusCode::BAD_REQUEST,
        };
        self.error_response_with_status(status)
    }
}

/// Return `BadRequest` for `JsonPayloadError`
impl WebResponseError<DefaultError> for error::JsonPayloadError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        let status = match self {
            error::JsonPayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        self.error_response_with_status(status)
    }
}

/// Error renderer for `PathError`
impl WebResponseError<DefaultError> for error::PathError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::NOT_FOUND)
    }
}

/// Error renderer `QueryPayloadError`
impl WebResponseError<DefaultError> for error::QueryPayloadError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

impl WebResponseError<DefaultError> for error::PayloadError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// `PayloadError` returns two possible results:
///
/// - `Overflow` returns `PayloadTooLarge`
/// - Other errors returns `BadRequest`
impl WebResponseError<DefaultError> for http::error::PayloadError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        let status = match self {
            http::error::PayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        self.error_response_with_status(status)
    }
}

#[cfg(feature = "cookie")]
/// Return `BadRequest` for `cookie::ParseError`
impl WebResponseError<DefaultError> for coo_kie::ParseError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// Return `BadRequest` for `ContentTypeError`
impl WebResponseError<DefaultError> for http::error::ContentTypeError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        self.error_response_with_status(StatusCode::BAD_REQUEST)
    }
}

/// Convert `ClientError` to a server `Response`
impl WebResponseError<DefaultError> for client::error::ClientError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        let status = match &self {
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
impl WebResponseError<DefaultError> for HandshakeError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        match self {
            HandshakeError::GetMethodRequired => HttpResponse::MethodNotAllowed()
                .header(header::ALLOW, "GET")
                .build(),
            HandshakeError::NoWebsocketUpgrade => HttpResponse::BadRequest()
                .reason("No WebSocket UPGRADE header found")
                .build(),
            HandshakeError::NoConnectionUpgrade => HttpResponse::BadRequest()
                .reason("No CONNECTION upgrade")
                .build(),
            HandshakeError::NoVersionHeader => HttpResponse::BadRequest()
                .reason("Websocket version header is required")
                .build(),
            HandshakeError::UnsupportedVersion => HttpResponse::BadRequest()
                .reason("Unsupported version")
                .build(),
            HandshakeError::BadWebsocketKey => {
                HttpResponse::BadRequest().reason("Handshake error").build()
            }
        }
    }
}

/// Return `GATEWAY_TIMEOUT` for `TimeoutError`
impl<E> WebResponseError<DefaultError> for TimeoutError<E>
where
    E: fmt::Display + fmt::Debug + Into<DefaultError> + 'static,
{
    fn error_response(self, req: &HttpRequest) -> HttpResponse {
        match self {
            TimeoutError::Service(e) => e.into().error_response(req),
            TimeoutError::Timeout => super::error::ErrorGatewayTimeout("").error_response(req),
        }
    }
}
