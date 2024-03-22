//! Http related errors
use std::{error, fmt, io, io::Write, str::Utf8Error, string::FromUtf8Error};

use ntex_h2::{self as h2};
use ntex_http::{header, uri::InvalidUri, StatusCode};

// re-export for convinience
pub use crate::channel::Canceled;
pub use ntex_http::error::Error as HttpError;

use crate::http::body::Body;
use crate::http::response::Response;
use crate::util::{BytesMut, Either};

/// Error that can be converted to `Response`
pub trait ResponseError: fmt::Display + fmt::Debug {
    /// Create response for error
    ///
    /// Internal server error is generated by default.
    fn error_response(&self) -> Response {
        let mut resp = Response::new(StatusCode::INTERNAL_SERVER_ERROR);
        let mut buf = BytesMut::new();
        let _ = write!(crate::http::helpers::Writer(&mut buf), "{}", self);
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        resp.set_body(Body::from(buf))
    }
}

impl<'a, T: ResponseError> ResponseError for &'a T {
    fn error_response(&self) -> Response {
        (*self).error_response()
    }
}

impl<T: ResponseError> From<T> for Response {
    fn from(err: T) -> Response {
        let resp = err.error_response();
        if resp.head().status == StatusCode::INTERNAL_SERVER_ERROR {
            log::error!("Internal Server Error: {:?}", err);
        } else {
            log::debug!("Error in response: {:?}", err);
        }
        resp
    }
}

/// Return `InternalServerError` for `HttpError`,
/// Response generation can return `HttpError`, so it is internal error
impl ResponseError for HttpError {}

/// Return `InternalServerError` for `io::Error`
impl ResponseError for io::Error {}

/// `InternalServerError` for `JsonError`
impl ResponseError for serde_json::error::Error {}

/// A set of errors that can occur during HTTP streams encoding
#[derive(thiserror::Error, Debug)]
pub enum EncodeError {
    /// An invalid `HttpVersion`, such as `HTP/1.1`
    #[error("Unsupported HTTP version specified")]
    UnsupportedVersion(super::Version),

    #[error("Unexpected end of bytes stream")]
    UnexpectedEof,

    /// Internal error
    #[error("Formater error {0}")]
    Fmt(io::Error),
}

/// A set of errors that can occur during parsing HTTP streams
#[derive(thiserror::Error, Debug)]
pub enum DecodeError {
    /// An invalid `Method`, such as `GE.T`.
    #[error("Invalid Method specified")]
    Method,
    /// An invalid `Uri`, such as `exam ple.domain`.
    #[error("Uri error: {0}")]
    Uri(#[from] InvalidUri),
    /// An invalid `HttpVersion`, such as `HTP/1.1`
    #[error("Invalid HTTP version specified")]
    Version,
    /// An invalid `Header`.
    #[error("Invalid Header provided")]
    Header,
    /// A message head is too large to be reasonable.
    #[error("Message head is too large")]
    TooLarge(usize),
    /// A message reached EOF, but is not complete.
    #[error("Message is incomplete")]
    Incomplete,
    /// An invalid `Status`, such as `1337 ELITE`.
    #[error("Invalid Status provided")]
    Status,
    /// An `InvalidInput` occurred while trying to parse incoming stream.
    #[error("`InvalidInput` occurred while trying to parse incoming stream: {0}")]
    InvalidInput(&'static str),
    /// Parsing a field as string failed
    #[error("UTF8 error: {0}")]
    Utf8(#[from] Utf8Error),
}

impl From<FromUtf8Error> for DecodeError {
    fn from(err: FromUtf8Error) -> DecodeError {
        DecodeError::Utf8(err.utf8_error())
    }
}

impl From<httparse::Error> for DecodeError {
    fn from(err: httparse::Error) -> DecodeError {
        match err {
            httparse::Error::HeaderName
            | httparse::Error::HeaderValue
            | httparse::Error::NewLine
            | httparse::Error::Token => DecodeError::Header,
            httparse::Error::Status => DecodeError::Status,
            httparse::Error::TooManyHeaders => DecodeError::TooLarge(0),
            httparse::Error::Version => DecodeError::Version,
        }
    }
}

#[derive(thiserror::Error, Debug)]
/// A set of errors that can occur during payload parsing
pub enum PayloadError {
    /// A payload reached EOF, but is not complete.
    #[error("A payload reached EOF, but is not complete. With error: {0:?}")]
    Incomplete(Option<io::Error>),
    /// Content encoding stream corruption
    #[error("Cannot decode content-encoding.")]
    EncodingCorrupted,
    /// A payload reached size limit.
    #[error("A payload reached size limit.")]
    Overflow,
    /// A payload length is unknown.
    #[error("A payload length is unknown.")]
    UnknownLength,
    /// Http2 payload error
    #[error("{0}")]
    Http2Payload(#[from] h2::StreamError),
    /// Decode error
    #[error("Decode error: {0}")]
    Decode(#[from] DecodeError),
    /// Io error
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl From<Either<PayloadError, io::Error>> for PayloadError {
    fn from(err: Either<PayloadError, io::Error>) -> Self {
        match err {
            Either::Left(err) => err,
            Either::Right(err) => PayloadError::Io(err),
        }
    }
}

#[derive(thiserror::Error, Debug)]
/// A set of errors that can occur during dispatching http requests
pub enum DispatchError {
    /// Service error
    #[error("Service error")]
    Service(Box<dyn super::ResponseError>),

    /// Control service error
    #[error("Control service error: {0}")]
    Control(Box<dyn std::error::Error>),
}

#[derive(thiserror::Error, Debug)]
/// A set of errors that can occur during dispatching http2 requests
pub enum H2Error {
    /// Operation error
    #[error("Operation error: {0}")]
    Operation(#[from] h2::OperationError),
    /// Pseudo headers error
    #[error("Missing pseudo header: {0}")]
    MissingPseudo(&'static str),
    /// Uri parsing error
    #[error("Uri: {0}")]
    Uri(#[from] InvalidUri),
    /// Body stream error
    #[error("{0}")]
    Stream(#[from] Box<dyn error::Error>),
}

/// A set of error that can occure during parsing content type
#[derive(thiserror::Error, PartialEq, Eq, Debug)]
pub enum ContentTypeError {
    /// Cannot parse content type
    #[error("Cannot parse content type")]
    ParseError,
    /// Unknown content encoding
    #[error("Unknown content encoding")]
    UnknownEncoding,
    /// Unexpected Content-Type
    #[error("Unexpected Content-Type")]
    Unexpected,
    /// Content-Type is expected
    #[error("Content-Type is expected")]
    Expected,
}

/// Blocking operation execution error
#[derive(thiserror::Error, Debug)]
pub enum BlockingError<E: fmt::Debug> {
    #[error("{0:?}")]
    Error(E),
    #[error("Thread pool is gone")]
    Canceled,
}

impl From<crate::rt::JoinError> for PayloadError {
    fn from(_: crate::rt::JoinError) -> Self {
        PayloadError::Io(io::Error::new(
            io::ErrorKind::Other,
            "Operation is canceled",
        ))
    }
}

impl From<BlockingError<io::Error>> for PayloadError {
    fn from(err: BlockingError<io::Error>) -> Self {
        match err {
            BlockingError::Error(e) => PayloadError::Io(e),
            BlockingError::Canceled => PayloadError::Io(io::Error::new(
                io::ErrorKind::Other,
                "Operation is canceled",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex_http::Error as HttpError;

    #[test]
    fn test_into_response() {
        let err: HttpError = StatusCode::from_u16(10000).err().unwrap().into();
        let resp: Response = err.error_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_error_http_response() {
        let orig = io::Error::new(io::ErrorKind::Other, "other");
        let resp: Response = orig.into();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_payload_error() {
        let err: PayloadError = io::Error::new(io::ErrorKind::Other, "DecodeError").into();
        assert!(format!("{}", err).contains("DecodeError"));

        let err: PayloadError = BlockingError::Canceled.into();
        assert!(format!("{}", err).contains("Operation is canceled"));

        let err: PayloadError =
            BlockingError::Error(io::Error::new(io::ErrorKind::Other, "DecodeError"))
                .into();
        assert!(format!("{}", err).contains("DecodeError"));

        let err = PayloadError::Incomplete(None);
        assert_eq!(
            format!("{}", err),
            "A payload reached EOF, but is not complete. With error: None"
        );
    }

    macro_rules! from {
        ($from:expr => $error:pat) => {
            match DecodeError::from($from) {
                e @ $error => {
                    assert!(format!("{}", e).len() >= 5);
                }
                e => unreachable!("{:?}", e),
            }
        };
    }

    #[test]
    fn test_from() {
        from!(httparse::Error::HeaderName => DecodeError::Header);
        from!(httparse::Error::HeaderName => DecodeError::Header);
        from!(httparse::Error::HeaderValue => DecodeError::Header);
        from!(httparse::Error::NewLine => DecodeError::Header);
        from!(httparse::Error::Status => DecodeError::Status);
        from!(httparse::Error::Token => DecodeError::Header);
        from!(httparse::Error::TooManyHeaders => DecodeError::TooLarge(0));
        from!(httparse::Error::Version => DecodeError::Version);
    }
}
