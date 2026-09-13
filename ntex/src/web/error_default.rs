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

use super::{HttpResponse, InternalError, WebResponseError, error};

#[derive(Debug, thiserror::Error)]
#[error("Default error marker")]
pub struct DefaultError {
    _ph: io::Error,
}

// =========== DefaultError impls

impl<St> WebResponseError<St, DefaultError> for DefaultError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        unreachable!()
    }
}

impl<St, T> WebResponseError<St, DefaultError> for InternalError<T>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    fn error_response(&mut self, _: &St) -> HttpResponse {
        crate::http::error::ResponseError::error_response(self)
    }
}

/// `InternalServerError` for `StateExtractorError`
impl<St> WebResponseError<St, DefaultError> for error::StateExtractorError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

/// `InternalServerError` for `JsonError`
impl<St> WebResponseError<St, DefaultError> for JsonError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

/// `InternalServerError` for `FormError`
impl<St> WebResponseError<St, DefaultError> for FormError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::Error`
impl<St> WebResponseError<St, DefaultError> for tls_openssl::ssl::Error {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::HandshakeError`
impl<St, T: fmt::Debug + 'static> WebResponseError<St, DefaultError>
    for tls_openssl::ssl::HandshakeError<T>
{
}

/// Return `BAD_REQUEST` for `de::value::Error`
impl<St> WebResponseError<St, DefaultError> for DeError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

/// `InternalServerError` for `Canceled`
impl<St> WebResponseError<St, DefaultError> for crate::http::error::Canceled {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::INTERNAL_SERVER_ERROR, self)
    }
}

/// `InternalServerError` for `BlockingError`
impl<St, E: Error + 'static> WebResponseError<St, DefaultError>
    for crate::http::error::BlockingError<E>
{
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::INTERNAL_SERVER_ERROR, self)
    }
}

/// Return `BAD_REQUEST` for `Utf8Error`
impl<St> WebResponseError<St, DefaultError> for Utf8Error {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

/// Return `InternalServerError` for `HttpError`,
/// Response generation can return `HttpError`, so it is internal error
impl<St> WebResponseError<St, DefaultError> for crate::http::error::HttpError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::INTERNAL_SERVER_ERROR, self)
    }
}

/// Return `InternalServerError` for `io::Error`
impl<St> WebResponseError<St, DefaultError> for io::Error {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        let status = match self.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        HttpResponse::render_with(status, self)
    }
}

/// `InternalServerError` for `UrlGeneratorError`
impl<St> WebResponseError<St, DefaultError> for error::UrlGenerationError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::INTERNAL_SERVER_ERROR, self)
    }
}

/// Response renderer for `UrlencodedError`
impl<St> WebResponseError<St, DefaultError> for error::UrlencodedError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        let status = match self {
            error::UrlencodedError::Overflow { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            error::UrlencodedError::UnknownLength => StatusCode::LENGTH_REQUIRED,
            _ => StatusCode::BAD_REQUEST,
        };
        HttpResponse::render_with(status, self)
    }
}

/// Return `BadRequest` for `JsonPayloadError`
impl<St> WebResponseError<St, DefaultError> for error::JsonPayloadError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        let status = match self {
            error::JsonPayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        HttpResponse::render_with(status, self)
    }
}

/// Error renderer for `PathError`
impl<St> WebResponseError<St, DefaultError> for error::PathError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::NOT_FOUND, self)
    }
}

/// Error renderer `QueryPayloadError`
impl<St> WebResponseError<St, DefaultError> for error::QueryPayloadError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

impl<St> WebResponseError<St, DefaultError> for error::PayloadError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

/// `PayloadError` returns two possible results:
///
/// - `Overflow` returns `PayloadTooLarge`
/// - Other errors returns `BadRequest`
impl<St> WebResponseError<St, DefaultError> for http::error::PayloadError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        let status = match self {
            http::error::PayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        };
        HttpResponse::render_with(status, self)
    }
}

#[cfg(feature = "cookie")]
/// Return `BadRequest` for `cookie::ParseError`
impl<St> WebResponseError<St, DefaultError> for coo_kie::ParseError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

/// Return `BadRequest` for `ContentTypeError`
impl<St> WebResponseError<St, DefaultError> for http::error::ContentTypeError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
        HttpResponse::render_with(StatusCode::BAD_REQUEST, self)
    }
}

/// Convert `ClientError` to a server `Response`
impl<St> WebResponseError<St, DefaultError> for client::error::ClientError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
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

        HttpResponse::render_with(status, self)
    }
}

#[cfg(feature = "ws")]
/// Error renderer for `ws::HandshakeError`
impl<St> WebResponseError<St, DefaultError> for HandshakeError {
    fn error_response(&mut self, _: &St) -> HttpResponse {
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
impl<St, E> WebResponseError<St, DefaultError> for TimeoutError<E>
where
    E: fmt::Display + fmt::Debug + WebResponseError<St, DefaultError> + 'static,
{
    fn error_response(&mut self, st: &St) -> HttpResponse {
        match self {
            TimeoutError::Service(e) => e.error_response(st),
            TimeoutError::Timeout => super::error::ErrorGatewayTimeout("").error_response(st),
        }
    }
}
