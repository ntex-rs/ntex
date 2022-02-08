//! Web error
use std::{fmt, io, str::Utf8Error};

use serde::de::value::Error as DeError;
use serde_json::error::Error as JsonError;
use serde_urlencoded::ser::Error as FormError;

use crate::http::{self, header, StatusCode};
use crate::util::timeout::TimeoutError;
use crate::ws::error::HandshakeError;

use super::error::{self, Error, ErrorRenderer};
use super::{HttpRequest, HttpResponse};

/// Default error type
#[derive(Clone, Copy, Default)]
pub struct DefaultError;

impl ErrorRenderer for DefaultError {}

/// Return `GATEWAY_TIMEOUT` for `TimeoutError`
impl<E: Error<DefaultError>> Error<DefaultError> for TimeoutError<E> {
    fn status_code(&self) -> StatusCode {
        match self {
            TimeoutError::Service(e) => e.status_code(),
            TimeoutError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }
}

/// `InternalServerError` for `DataExtractorError`
impl Error<DefaultError> for error::DataExtractorError {}

/// `InternalServerError` for `JsonError`
impl Error<DefaultError> for JsonError {}

/// `InternalServerError` for `FormError`
impl Error<DefaultError> for FormError {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::Error`
impl Error<DefaultError> for tls_openssl::ssl::Error {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::HandshakeError`
impl<T: fmt::Debug + 'static> Error<DefaultError> for tls_openssl::ssl::HandshakeError<T> {}

/// Return `BAD_REQUEST` for `de::value::Error`
impl Error<DefaultError> for DeError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// `InternalServerError` for `Canceled`
impl Error<DefaultError> for crate::http::error::Canceled {}

/// `InternalServerError` for `BlockingError`
impl<E: fmt::Debug + 'static> Error<DefaultError> for crate::http::error::BlockingError<E> {}

/// Return `BAD_REQUEST` for `Utf8Error`
impl Error<DefaultError> for Utf8Error {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Return `InternalServerError` for `HttpError`,
/// Response generation can return `HttpError`, so it is internal error
impl Error<DefaultError> for crate::http::error::HttpError {}

/// Return `InternalServerError` for `io::Error`
impl Error<DefaultError> for io::Error {
    fn status_code(&self) -> StatusCode {
        match self.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// `InternalServerError` for `UrlGeneratorError`
impl Error<DefaultError> for error::UrlGenerationError {}

/// Response renderer for `UrlencodedError`
impl Error<DefaultError> for error::UrlencodedError {
    fn status_code(&self) -> StatusCode {
        match *self {
            error::UrlencodedError::Overflow { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            error::UrlencodedError::UnknownLength => StatusCode::LENGTH_REQUIRED,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// Return `BadRequest` for `JsonPayloadError`
impl Error<DefaultError> for error::JsonPayloadError {
    fn status_code(&self) -> StatusCode {
        match *self {
            error::JsonPayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// Error renderer for `PathError`
impl Error<DefaultError> for error::PathError {
    fn status_code(&self) -> StatusCode {
        StatusCode::NOT_FOUND
    }
}

/// Error renderer `QueryPayloadError`
impl Error<DefaultError> for error::QueryPayloadError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl Error<DefaultError> for error::PayloadError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// `PayloadError` returns two possible results:
///
/// - `Overflow` returns `PayloadTooLarge`
/// - Other errors returns `BadRequest`
impl Error<DefaultError> for http::error::PayloadError {
    fn status_code(&self) -> StatusCode {
        match *self {
            http::error::PayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

#[cfg(feature = "cookie")]
/// Return `BadRequest` for `cookie::ParseError`
impl Error<DefaultError> for coo_kie::ParseError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Return `BadRequest` for `ContentTypeError`
impl Error<DefaultError> for http::error::ContentTypeError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Convert `SendRequestError` to a server `Response`
impl Error<DefaultError> for http::client::error::SendRequestError {
    fn status_code(&self) -> StatusCode {
        match *self {
            http::client::error::SendRequestError::Connect(
                http::client::error::ConnectError::Timeout,
            ) => StatusCode::GATEWAY_TIMEOUT,
            http::client::error::SendRequestError::Connect(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Error renderer for ws::HandshakeError
impl Error<DefaultError> for HandshakeError {
    fn error_response(self, _: &HttpRequest) -> HttpResponse {
        match self {
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
