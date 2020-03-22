//! Error and Result module
use ntex::http::StatusCode;
use ntex::http::error::{PayloadError, ParseError};
use ntex::web::WebResponseError;
use derive_more::{Display, From};

/// A set of errors that can occur during parsing multipart streams
#[derive(Debug, Display, From)]
pub enum MultipartError {
    /// Content-Type header is not found
    #[display(fmt = "No Content-type header found")]
    NoContentType,
    /// Can not parse Content-Type header
    #[display(fmt = "Can not parse Content-Type header")]
    ParseContentType,
    /// Multipart boundary is not found
    #[display(fmt = "Multipart boundary is not found")]
    Boundary,
    /// Nested multipart is not supported
    #[display(fmt = "Nested multipart is not supported")]
    Nested,
    /// Multipart stream is incomplete
    #[display(fmt = "Multipart stream is incomplete")]
    Incomplete,
    /// Error during field parsing
    #[display(fmt = "{}", _0)]
    Parse(ParseError),
    /// Payload error
    #[display(fmt = "{}", _0)]
    Payload(PayloadError),
    /// Not consumed
    #[display(fmt = "Multipart stream is not consumed")]
    NotConsumed,
}

impl std::error::Error for MultipartError {}

/// Return `BadRequest` for `MultipartError`
impl<Err> WebResponseError<Err> for MultipartError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
}
