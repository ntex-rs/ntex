//! Web error
use std::{fmt, io, io::Write, str::Utf8Error};

use serde::de::value::Error as DeError;
use serde_json::error::Error as JsonError;
use serde_urlencoded::ser::Error as FormError;

use crate::client;
use crate::http::{self, StatusCode, body::Body, header};
use crate::util::{BytesMut, timeout::TimeoutError};
#[cfg(feature = "ws")]
use crate::ws::error::HandshakeError;

use super::error::{self, ErrorContainer, WebResponseError};
use super::{HttpRequest, HttpResponse};

/// Generic error container for errors that supports `DefaultError` renderer.
#[derive(thiserror::Error)]
pub struct WebError {
    cause: Box<dyn WebResponseError>,
}

impl WebError {
    pub fn new(err: impl WebResponseError + 'static) -> WebError {
        WebError {
            cause: Box::new(err),
        }
    }

    /// Returns the reference to the underlying `WebResponseError`.
    pub fn as_response_error(&self) -> &dyn WebResponseError {
        self.cause.as_ref()
    }
}

/// `Error` for any error which implements `WebResponseError<DefaultError>`
impl<T: WebResponseError> From<T> for WebError {
    fn from(err: T) -> Self {
        WebError {
            cause: Box::new(err),
        }
    }
}

impl ErrorContainer for WebError {
    fn error_response(&self, req: &HttpRequest) -> HttpResponse {
        self.cause.error_response(req)
    }
}

impl crate::http::error::ResponseError for WebError {
    fn error_response(&self) -> HttpResponse {
        let mut resp = HttpResponse::new(self.cause.status_code());
        let mut buf = BytesMut::new();
        let _ = write!(&mut buf, "{}", self.cause);
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        resp.set_body(Body::from(buf))
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

/// Return `GATEWAY_TIMEOUT` for `TimeoutError`
impl<E> From<TimeoutError<E>> for WebError
where
    WebError: From<E>,
{
    fn from(err: TimeoutError<E>) -> WebError {
        match err {
            TimeoutError::Service(e) => e.into(),
            TimeoutError::Timeout => super::error::ErrorGatewayTimeout("").into(),
        }
    }
}

/// `InternalServerError` for `StateExtractorError`
impl WebResponseError for error::StateExtractorError {}

/// `InternalServerError` for `JsonError`
impl WebResponseError for JsonError {}

/// `InternalServerError` for `FormError`
impl WebResponseError for FormError {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::Error`
impl WebResponseError for tls_openssl::ssl::Error {}

#[cfg(feature = "openssl")]
/// `InternalServerError` for `openssl::ssl::HandshakeError`
impl<T: fmt::Debug + 'static> WebResponseError for tls_openssl::ssl::HandshakeError<T> {}

/// Return `BAD_REQUEST` for `de::value::Error`
impl WebResponseError for DeError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// `InternalServerError` for `Canceled`
impl WebResponseError for crate::http::error::Canceled {}

/// `InternalServerError` for `BlockingError`
impl<E: fmt::Debug + 'static> WebResponseError for crate::http::error::BlockingError<E> {}

/// Return `BAD_REQUEST` for `Utf8Error`
impl WebResponseError for Utf8Error {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Return `InternalServerError` for `HttpError`,
/// Response generation can return `HttpError`, so it is internal error
impl WebResponseError for crate::http::error::HttpError {}

/// Return `InternalServerError` for `io::Error`
impl WebResponseError for io::Error {
    fn status_code(&self) -> StatusCode {
        match self.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// `InternalServerError` for `UrlGeneratorError`
impl WebResponseError for error::UrlGenerationError {}

/// Response renderer for `UrlencodedError`
impl WebResponseError for error::UrlencodedError {
    fn status_code(&self) -> StatusCode {
        match *self {
            error::UrlencodedError::Overflow { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            error::UrlencodedError::UnknownLength => StatusCode::LENGTH_REQUIRED,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// Return `BadRequest` for `JsonPayloadError`
impl WebResponseError for error::JsonPayloadError {
    fn status_code(&self) -> StatusCode {
        match *self {
            error::JsonPayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// Error renderer for `PathError`
impl WebResponseError for error::PathError {
    fn status_code(&self) -> StatusCode {
        StatusCode::NOT_FOUND
    }
}

/// Error renderer `QueryPayloadError`
impl WebResponseError for error::QueryPayloadError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

impl WebResponseError for error::PayloadError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// `PayloadError` returns two possible results:
///
/// - `Overflow` returns `PayloadTooLarge`
/// - Other errors returns `BadRequest`
impl WebResponseError for http::error::PayloadError {
    fn status_code(&self) -> StatusCode {
        match *self {
            http::error::PayloadError::Overflow => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

#[cfg(feature = "cookie")]
/// Return `BadRequest` for `cookie::ParseError`
impl WebResponseError for coo_kie::ParseError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Return `BadRequest` for `ContentTypeError`
impl WebResponseError for http::error::ContentTypeError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}

/// Convert `ClientError` to a server `Response`
impl WebResponseError for client::error::ClientError {
    fn status_code(&self) -> StatusCode {
        match self {
            client::error::ClientError::Connect(err) => {
                if matches!(err, client::error::ConnectError::Timeout) {
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
impl WebResponseError for HandshakeError {
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
