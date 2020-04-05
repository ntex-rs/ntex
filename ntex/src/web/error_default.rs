//! Web error
use std::str::Utf8Error;
use std::{fmt, io};

use serde::de::value::Error as DeError;
use serde_json::error::Error as JsonError;
use serde_urlencoded::ser::Error as FormError;

use crate::http::StatusCode;
use crate::util::timeout::TimeoutError;

use super::error::{self, ErrorRenderer, WebResponseError};
use super::HttpResponse;

/// Default error type
#[derive(Clone, Copy, Default)]
pub struct DefaultError;

/// Generic error container for errors that supports `DefaultError` renderer.
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

impl ErrorRenderer for DefaultError {
    type Container = Error;
}

/// `Error` for any error which implements `WebResponseError<DefaultError>`
impl<T: WebResponseError<DefaultError>> From<T> for Error {
    fn from(err: T) -> Self {
        Error {
            cause: Box::new(err),
        }
    }
}

impl crate::http::error::ResponseError for Error {
    fn error_response(&self) -> HttpResponse {
        self.cause.error_response()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause, f)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.cause, f)
    }
}

/// Return `GATEWAY_TIMEOUT` for `TimeoutError`
impl<E: WebResponseError<DefaultError>> WebResponseError<DefaultError>
    for TimeoutError<E>
{
    fn status_code(&self) -> StatusCode {
        match self {
            TimeoutError::Service(e) => e.status_code(),
            TimeoutError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }
}

/// `InternalServerError` for `Infallible`
impl WebResponseError<DefaultError> for std::convert::Infallible {}

/// `InternalServerError` for `DataExtractorError`
impl WebResponseError<DefaultError> for error::DataExtractorError {}

/// `InternalServerError` for `JsonError`
impl WebResponseError<DefaultError> for JsonError {}

/// `InternalServerError` for `FormError`
impl WebResponseError<DefaultError> for FormError {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::Error`
impl WebResponseError<DefaultError> for crate::connect::openssl::SslError {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::HandshakeError`
impl<T: std::fmt::Debug + 'static> WebResponseError<DefaultError>
    for crate::server::openssl::HandshakeError<T>
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
