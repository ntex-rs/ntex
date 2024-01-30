//! Web error
use std::{cell::RefCell, fmt, io::Write, marker::PhantomData};

use thiserror::Error;

pub use ntex_http::error::Error as HttpError;
pub use serde_json::error::Error as JsonError;
#[cfg(feature = "url")]
pub use url_pkg::ParseError as UrlParseError;

use crate::http::body::Body;
use crate::http::helpers::Writer;
use crate::http::{error, header, StatusCode};
use crate::util::{BytesMut, Either};

pub use super::error_default::{DefaultError, Error};
pub use crate::http::error::BlockingError;

use super::{HttpRequest, HttpResponse};

pub trait ErrorRenderer: Sized + 'static {
    type Container: ErrorContainer;
}

pub trait ErrorContainer: error::ResponseError + Sized {
    /// Generate response for error container
    fn error_response(&self, req: &HttpRequest) -> HttpResponse;
}

/// Error that can be rendered to a `Response`
pub trait WebResponseError<Err = DefaultError>:
    fmt::Display + fmt::Debug + 'static
where
    Err: ErrorRenderer,
{
    /// Response's status code
    ///
    /// Internal server error is generated by default.
    fn status_code(&self) -> StatusCode {
        StatusCode::INTERNAL_SERVER_ERROR
    }

    /// Generate response for error
    ///
    /// Internal server error is generated by default.
    fn error_response(&self, _: &HttpRequest) -> HttpResponse {
        let mut resp = HttpResponse::new(self.status_code());
        let mut buf = BytesMut::new();
        let _ = write!(Writer(&mut buf), "{}", self);
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        resp.set_body(Body::from(buf))
    }
}

impl<Err: ErrorRenderer> WebResponseError<Err> for std::convert::Infallible {}

impl<A, B, Err> WebResponseError<Err> for Either<A, B>
where
    A: WebResponseError<Err>,
    B: WebResponseError<Err>,
    Err: ErrorRenderer,
{
    fn status_code(&self) -> StatusCode {
        match self {
            Either::Left(ref a) => a.status_code(),
            Either::Right(ref b) => b.status_code(),
        }
    }

    fn error_response(&self, req: &HttpRequest) -> HttpResponse {
        match self {
            Either::Left(ref a) => a.error_response(req),
            Either::Right(ref b) => b.error_response(req),
        }
    }
}

/// Errors which can occur when attempting to work with `State` extractor
#[derive(Error, Debug, Copy, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct AppFactoryError(pub &'static str);

/// Errors which can occur when attempting to work with `State` extractor
#[derive(Error, Debug, Copy, Clone, PartialEq, Eq)]
pub enum StateExtractorError {
    #[error("App state is not configured, to configure use App::state()")]
    NotConfigured,
}

/// Errors which can occur when attempting to generate resource uri.
#[derive(Error, Debug, Copy, Clone, PartialEq, Eq)]
pub enum UrlGenerationError {
    /// Resource not found
    #[error("Resource not found")]
    ResourceNotFound,
    /// Not all path pattern covered
    #[error("Not all path pattern covered")]
    NotEnoughElements,
    /// URL parse error
    #[cfg(feature = "url")]
    #[error("{0}")]
    ParseError(#[from] UrlParseError),
}

/// A set of errors that can occur during parsing urlencoded payloads
#[derive(Error, Debug)]
pub enum UrlencodedError {
    /// Cannot decode chunked transfer encoding
    #[error("Cannot decode chunked transfer encoding")]
    Chunked,
    /// Payload size is bigger than allowed. (default: 256kB)
    #[error(
        "Urlencoded payload size is bigger ({size} bytes) than allowed (default: {limit} bytes)",
    )]
    Overflow { size: usize, limit: usize },
    /// Payload size is unknown
    #[error("Payload size is unknown")]
    UnknownLength,
    /// Content type error
    #[error("Content type error")]
    ContentType,
    /// Parse error
    #[error("Parse error")]
    Parse,
    /// Payload error
    #[error("Error that occur during reading payload: {0}")]
    Payload(#[from] error::PayloadError),
}

/// A set of errors that can occur during parsing json payloads
#[derive(Error, Debug)]
pub enum JsonPayloadError {
    /// Payload size is bigger than allowed. (default: 32kB)
    #[error("Json payload size is bigger than allowed")]
    Overflow,
    /// Content type error
    #[error("Content type error")]
    ContentType,
    /// Deserialize error
    #[error("Json deserialize error: {0}")]
    Deserialize(#[from] serde_json::error::Error),
    /// Payload error
    #[error("Error that occur during reading payload: {0}")]
    Payload(#[from] error::PayloadError),
}

/// A set of errors that can occur during parsing request paths
#[derive(Error, Debug)]
pub enum PathError {
    /// Deserialize error
    #[error("Path deserialize error: {0}")]
    Deserialize(#[from] serde::de::value::Error),
}

/// A set of errors that can occur during parsing query strings
#[derive(Error, Debug)]
pub enum QueryPayloadError {
    /// Deserialize error
    #[error("Query deserialize error: {0}")]
    Deserialize(#[from] serde::de::value::Error),
}

#[derive(Error, Debug)]
pub enum PayloadError {
    /// Http error.
    #[error("{0:?}")]
    Http(#[from] error::HttpError),
    #[error("{0}")]
    Payload(#[from] error::PayloadError),
    #[error("{0}")]
    ContentType(#[from] error::ContentTypeError),
    #[error("Cannot decode body")]
    Decoding,
}

/// Helper type that can wrap any error and generate custom response.
///
/// In following example any `io::Error` will be converted into "BAD REQUEST"
/// response as opposite to *INTERNAL SERVER ERROR* which is defined by
/// default.
///
/// ```rust
/// use ntex::http::Request;
///
/// fn index(req: Request) -> Result<&'static str, std::io::Error> {
///     Err(std::io::Error::new(std::io::ErrorKind::Other, "error"))
/// }
/// ```
pub struct InternalError<T, Err = DefaultError> {
    cause: T,
    status: InternalErrorType,
    _t: PhantomData<Err>,
}

enum InternalErrorType {
    Status(StatusCode),
    Response(RefCell<Option<HttpResponse>>),
}

impl<T> InternalError<T> {
    /// Create `InternalError` instance
    pub fn default(cause: T, status: StatusCode) -> Self {
        InternalError {
            cause,
            status: InternalErrorType::Status(status),
            _t: PhantomData,
        }
    }
}

impl<T, Err> InternalError<T, Err> {
    /// Create `InternalError` instance
    pub fn new(cause: T, status: StatusCode) -> Self {
        InternalError {
            cause,
            status: InternalErrorType::Status(status),
            _t: PhantomData,
        }
    }

    /// Create `InternalError` with predefined `Response`.
    pub fn from_response(cause: T, response: HttpResponse) -> Self {
        InternalError {
            cause,
            status: InternalErrorType::Response(RefCell::new(Some(response))),
            _t: PhantomData,
        }
    }
}

impl<T, E> fmt::Debug for InternalError<T, E>
where
    T: fmt::Debug + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "web::InternalError({:?})", &self.cause)
    }
}

impl<T, E> fmt::Display for InternalError<T, E>
where
    T: fmt::Display + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.cause, f)
    }
}

impl<T: fmt::Display + fmt::Debug + 'static, E> std::error::Error for InternalError<T, E> {}

impl<T, E> WebResponseError<E> for InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
    E: ErrorRenderer,
{
    fn error_response(&self, _: &HttpRequest) -> HttpResponse {
        crate::http::error::ResponseError::error_response(self)
    }
}

impl<T, E> crate::http::error::ResponseError for InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
    E: ErrorRenderer,
{
    fn error_response(&self) -> HttpResponse {
        match self.status {
            InternalErrorType::Status(st) => {
                let mut res = HttpResponse::new(st);
                let mut buf = BytesMut::new();
                let _ = write!(Writer(&mut buf), "{}", self);
                res.headers_mut().insert(
                    header::CONTENT_TYPE,
                    header::HeaderValue::from_static("text/plain; charset=utf-8"),
                );
                res.set_body(Body::from(buf))
            }
            InternalErrorType::Response(ref resp) => {
                if let Some(resp) = resp.borrow_mut().take() {
                    resp
                } else {
                    HttpResponse::new(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
    }
}

/// Helper function that creates wrapper of any error and generate *BAD
/// REQUEST* response.
#[allow(non_snake_case)]
pub fn ErrorBadRequest<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::BAD_REQUEST)
}

/// Helper function that creates wrapper of any error and generate
/// *UNAUTHORIZED* response.
#[allow(non_snake_case)]
pub fn ErrorUnauthorized<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::UNAUTHORIZED)
}

/// Helper function that creates wrapper of any error and generate
/// *PAYMENT_REQUIRED* response.
#[allow(non_snake_case)]
pub fn ErrorPaymentRequired<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::PAYMENT_REQUIRED)
}

/// Helper function that creates wrapper of any error and generate *FORBIDDEN*
/// response.
#[allow(non_snake_case)]
pub fn ErrorForbidden<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::FORBIDDEN)
}

/// Helper function that creates wrapper of any error and generate *NOT FOUND*
/// response.
#[allow(non_snake_case)]
pub fn ErrorNotFound<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::NOT_FOUND)
}

/// Helper function that creates wrapper of any error and generate *METHOD NOT
/// ALLOWED* response.
#[allow(non_snake_case)]
pub fn ErrorMethodNotAllowed<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::METHOD_NOT_ALLOWED)
}

/// Helper function that creates wrapper of any error and generate *NOT
/// ACCEPTABLE* response.
#[allow(non_snake_case)]
pub fn ErrorNotAcceptable<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::NOT_ACCEPTABLE)
}

/// Helper function that creates wrapper of any error and generate *PROXY
/// AUTHENTICATION REQUIRED* response.
#[allow(non_snake_case)]
pub fn ErrorProxyAuthenticationRequired<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::PROXY_AUTHENTICATION_REQUIRED)
}

/// Helper function that creates wrapper of any error and generate *REQUEST
/// TIMEOUT* response.
#[allow(non_snake_case)]
pub fn ErrorRequestTimeout<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::REQUEST_TIMEOUT)
}

/// Helper function that creates wrapper of any error and generate *CONFLICT*
/// response.
#[allow(non_snake_case)]
pub fn ErrorConflict<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::CONFLICT)
}

/// Helper function that creates wrapper of any error and generate *GONE*
/// response.
#[allow(non_snake_case)]
pub fn ErrorGone<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::GONE)
}

/// Helper function that creates wrapper of any error and generate *LENGTH
/// REQUIRED* response.
#[allow(non_snake_case)]
pub fn ErrorLengthRequired<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::LENGTH_REQUIRED)
}

/// Helper function that creates wrapper of any error and generate
/// *PAYLOAD TOO LARGE* response.
#[allow(non_snake_case)]
pub fn ErrorPayloadTooLarge<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::PAYLOAD_TOO_LARGE)
}

/// Helper function that creates wrapper of any error and generate
/// *URI TOO LONG* response.
#[allow(non_snake_case)]
pub fn ErrorUriTooLong<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::URI_TOO_LONG)
}

/// Helper function that creates wrapper of any error and generate
/// *UNSUPPORTED MEDIA TYPE* response.
#[allow(non_snake_case)]
pub fn ErrorUnsupportedMediaType<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::UNSUPPORTED_MEDIA_TYPE)
}

/// Helper function that creates wrapper of any error and generate
/// *RANGE NOT SATISFIABLE* response.
#[allow(non_snake_case)]
pub fn ErrorRangeNotSatisfiable<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::RANGE_NOT_SATISFIABLE)
}

/// Helper function that creates wrapper of any error and generate
/// *IM A TEAPOT* response.
#[allow(non_snake_case)]
pub fn ErrorImATeapot<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::IM_A_TEAPOT)
}

/// Helper function that creates wrapper of any error and generate
/// *MISDIRECTED REQUEST* response.
#[allow(non_snake_case)]
pub fn ErrorMisdirectedRequest<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::MISDIRECTED_REQUEST)
}

/// Helper function that creates wrapper of any error and generate
/// *UNPROCESSABLE ENTITY* response.
#[allow(non_snake_case)]
pub fn ErrorUnprocessableEntity<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::UNPROCESSABLE_ENTITY)
}

/// Helper function that creates wrapper of any error and generate
/// *LOCKED* response.
#[allow(non_snake_case)]
pub fn ErrorLocked<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::LOCKED)
}

/// Helper function that creates wrapper of any error and generate
/// *FAILED DEPENDENCY* response.
#[allow(non_snake_case)]
pub fn ErrorFailedDependency<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::FAILED_DEPENDENCY)
}

/// Helper function that creates wrapper of any error and generate
/// *UPGRADE REQUIRED* response.
#[allow(non_snake_case)]
pub fn ErrorUpgradeRequired<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::UPGRADE_REQUIRED)
}

/// Helper function that creates wrapper of any error and generate
/// *PRECONDITION FAILED* response.
#[allow(non_snake_case)]
pub fn ErrorPreconditionFailed<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::PRECONDITION_FAILED)
}

/// Helper function that creates wrapper of any error and generate
/// *PRECONDITION REQUIRED* response.
#[allow(non_snake_case)]
pub fn ErrorPreconditionRequired<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::PRECONDITION_REQUIRED)
}

/// Helper function that creates wrapper of any error and generate
/// *TOO MANY REQUESTS* response.
#[allow(non_snake_case)]
pub fn ErrorTooManyRequests<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::TOO_MANY_REQUESTS)
}

/// Helper function that creates wrapper of any error and generate
/// *REQUEST HEADER FIELDS TOO LARGE* response.
#[allow(non_snake_case)]
pub fn ErrorRequestHeaderFieldsTooLarge<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE)
}

/// Helper function that creates wrapper of any error and generate
/// *UNAVAILABLE FOR LEGAL REASONS* response.
#[allow(non_snake_case)]
pub fn ErrorUnavailableForLegalReasons<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS)
}

/// Helper function that creates wrapper of any error and generate
/// *EXPECTATION FAILED* response.
#[allow(non_snake_case)]
pub fn ErrorExpectationFailed<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::EXPECTATION_FAILED)
}

/// Helper function that creates wrapper of any error and
/// generate *INTERNAL SERVER ERROR* response.
#[allow(non_snake_case)]
pub fn ErrorInternalServerError<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::INTERNAL_SERVER_ERROR)
}

/// Helper function that creates wrapper of any error and
/// generate *NOT IMPLEMENTED* response.
#[allow(non_snake_case)]
pub fn ErrorNotImplemented<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::NOT_IMPLEMENTED)
}

/// Helper function that creates wrapper of any error and
/// generate *BAD GATEWAY* response.
#[allow(non_snake_case)]
pub fn ErrorBadGateway<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::BAD_GATEWAY)
}

/// Helper function that creates wrapper of any error and
/// generate *SERVICE UNAVAILABLE* response.
#[allow(non_snake_case)]
pub fn ErrorServiceUnavailable<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::SERVICE_UNAVAILABLE)
}

/// Helper function that creates wrapper of any error and
/// generate *GATEWAY TIMEOUT* response.
#[allow(non_snake_case)]
pub fn ErrorGatewayTimeout<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::GATEWAY_TIMEOUT)
}

/// Helper function that creates wrapper of any error and
/// generate *HTTP VERSION NOT SUPPORTED* response.
#[allow(non_snake_case)]
pub fn ErrorHttpVersionNotSupported<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::HTTP_VERSION_NOT_SUPPORTED)
}

/// Helper function that creates wrapper of any error and
/// generate *VARIANT ALSO NEGOTIATES* response.
#[allow(non_snake_case)]
pub fn ErrorVariantAlsoNegotiates<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::VARIANT_ALSO_NEGOTIATES)
}

/// Helper function that creates wrapper of any error and
/// generate *INSUFFICIENT STORAGE* response.
#[allow(non_snake_case)]
pub fn ErrorInsufficientStorage<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::INSUFFICIENT_STORAGE)
}

/// Helper function that creates wrapper of any error and
/// generate *LOOP DETECTED* response.
#[allow(non_snake_case)]
pub fn ErrorLoopDetected<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::LOOP_DETECTED)
}

/// Helper function that creates wrapper of any error and
/// generate *NOT EXTENDED* response.
#[allow(non_snake_case)]
pub fn ErrorNotExtended<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::NOT_EXTENDED)
}

/// Helper function that creates wrapper of any error and
/// generate *NETWORK AUTHENTICATION REQUIRED* response.
#[allow(non_snake_case)]
pub fn ErrorNetworkAuthenticationRequired<T, E>(err: T) -> InternalError<T, E>
where
    T: fmt::Debug + fmt::Display + 'static,
{
    InternalError::new(err, StatusCode::NETWORK_AUTHENTICATION_REQUIRED)
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;
    use crate::http::client::error::{ConnectError, SendRequestError};
    use crate::web::test::TestRequest;
    use crate::web::DefaultError;

    #[test]
    fn test_into_error() {
        let err = UrlencodedError::UnknownLength;
        let e: Error = err.into();
        let s = format!("{}", e);
        assert!(s.contains("Payload size is unknown"));

        let e = Error::new(UrlencodedError::UnknownLength);
        let s = format!("{:?}", e);
        assert!(s.contains("UnknownLength"));

        let res = crate::http::ResponseError::error_response(&e);
        assert_eq!(res.status(), StatusCode::LENGTH_REQUIRED);
        assert_eq!(
            e.as_response_error().status_code(),
            StatusCode::LENGTH_REQUIRED
        )
    }

    #[test]
    fn test_other_errors() {
        let req = TestRequest::default().to_http_request();

        use crate::util::timeout::TimeoutError;
        let resp = WebResponseError::<DefaultError>::error_response(
            &TimeoutError::<UrlencodedError>::Timeout,
            &req,
        );
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);

        let resp = WebResponseError::<DefaultError>::error_response(
            &SendRequestError::Connect(ConnectError::Timeout),
            &req,
        );
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);

        let resp = WebResponseError::<DefaultError>::error_response(
            &SendRequestError::Connect(ConnectError::SslIsNotSupported),
            &req,
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let resp = WebResponseError::<DefaultError>::error_response(
            &SendRequestError::TunnelNotSupported,
            &req,
        );
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        #[cfg(feature = "cookie")]
        {
            let resp: HttpResponse = WebResponseError::<DefaultError>::error_response(
                &coo_kie::ParseError::EmptyName,
                &req,
            );
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }

        let resp = WebResponseError::<DefaultError>::error_response(
            &crate::http::error::ContentTypeError::ParseError,
            &req,
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let err = serde_urlencoded::from_str::<i32>("bad query").unwrap_err();
        let resp = WebResponseError::<DefaultError>::error_response(&err, &req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let err = PayloadError::Decoding;
        let resp = WebResponseError::<DefaultError>::error_response(&err, &req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_either_error() {
        let req = TestRequest::default().to_http_request();

        let err: Either<SendRequestError, PayloadError> =
            Either::Left(SendRequestError::TunnelNotSupported);
        let code = WebResponseError::<DefaultError>::status_code(&err);
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        let resp = WebResponseError::<DefaultError>::error_response(&err, &req);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let err: Either<SendRequestError, PayloadError> =
            Either::Right(PayloadError::Decoding);
        let code = WebResponseError::<DefaultError>::status_code(&err);
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let resp = WebResponseError::<DefaultError>::error_response(&err, &req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_io_error() {
        assert_eq!(
            StatusCode::NOT_FOUND,
            WebResponseError::<DefaultError>::status_code(&io::Error::new(
                io::ErrorKind::NotFound,
                ""
            )),
        );
        assert_eq!(
            StatusCode::FORBIDDEN,
            WebResponseError::<DefaultError>::status_code(&io::Error::new(
                io::ErrorKind::PermissionDenied,
                ""
            )),
        );
        assert_eq!(
            StatusCode::INTERNAL_SERVER_ERROR,
            WebResponseError::<DefaultError>::status_code(&io::Error::new(
                io::ErrorKind::Other,
                ""
            )),
        );
    }

    #[test]
    fn test_urlencoded_error() {
        let req = TestRequest::default().to_http_request();
        let resp: HttpResponse = WebResponseError::<DefaultError>::error_response(
            &UrlencodedError::Overflow { size: 0, limit: 0 },
            &req,
        );
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let resp: HttpResponse = WebResponseError::<DefaultError>::error_response(
            &UrlencodedError::UnknownLength,
            &req,
        );
        assert_eq!(resp.status(), StatusCode::LENGTH_REQUIRED);
        let resp: HttpResponse = WebResponseError::<DefaultError>::error_response(
            &UrlencodedError::ContentType,
            &req,
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_json_payload_error() {
        let req = TestRequest::default().to_http_request();
        let resp: HttpResponse = WebResponseError::<DefaultError>::error_response(
            &JsonPayloadError::Overflow,
            &req,
        );
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let resp: HttpResponse = WebResponseError::<DefaultError>::error_response(
            &JsonPayloadError::ContentType,
            &req,
        );
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn test_query_payload_error() {
        let req = TestRequest::default().to_http_request();

        let err = QueryPayloadError::Deserialize(
            serde_urlencoded::from_str::<i32>("bad query").unwrap_err(),
        );

        let resp: HttpResponse =
            WebResponseError::<DefaultError>::error_response(&err, &req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            WebResponseError::<DefaultError>::status_code(&err),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_path_error() {
        let req = TestRequest::default().to_http_request();
        let err = PathError::Deserialize(
            serde_urlencoded::from_str::<i32>("bad path").unwrap_err(),
        );
        let resp: HttpResponse =
            WebResponseError::<DefaultError>::error_response(&err, &req);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            WebResponseError::<DefaultError>::status_code(&err),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn test_handshake_error() {
        use crate::ws::error::HandshakeError;

        let req = TestRequest::default().to_http_request();

        let resp = HandshakeError::GetMethodRequired.error_response(&req);
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        let resp = HandshakeError::NoWebsocketUpgrade.error_response(&req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = HandshakeError::NoConnectionUpgrade.error_response(&req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = HandshakeError::NoVersionHeader.error_response(&req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = HandshakeError::UnsupportedVersion.error_response(&req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = HandshakeError::BadWebsocketKey.error_response(&req);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn test_error_helpers() {
        let err = ErrorBadRequest::<_, DefaultError>("err");
        assert!(format!("{:?}", err).contains("web::InternalError"));

        let err: InternalError<_, DefaultError> =
            InternalError::from_response("err", HttpResponse::BadRequest().finish());
        let r: HttpResponse = err.into();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        let r: HttpResponse = ErrorBadRequest::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        let r: HttpResponse = ErrorUnauthorized::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        let r: HttpResponse = ErrorPaymentRequired::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::PAYMENT_REQUIRED);

        let r: HttpResponse = ErrorForbidden::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        let r: HttpResponse = ErrorNotFound::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);

        let r: HttpResponse = ErrorMethodNotAllowed::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);

        let r: HttpResponse = ErrorNotAcceptable::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::NOT_ACCEPTABLE);

        let r: HttpResponse =
            ErrorProxyAuthenticationRequired::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::PROXY_AUTHENTICATION_REQUIRED);

        let r: HttpResponse = ErrorRequestTimeout::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::REQUEST_TIMEOUT);

        let r: HttpResponse = ErrorConflict::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::CONFLICT);

        let r: HttpResponse = ErrorGone::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::GONE);

        let r: HttpResponse = ErrorLengthRequired::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::LENGTH_REQUIRED);

        let r: HttpResponse = ErrorPreconditionFailed::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::PRECONDITION_FAILED);

        let r: HttpResponse = ErrorPayloadTooLarge::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let r: HttpResponse = ErrorUriTooLong::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::URI_TOO_LONG);

        let r: HttpResponse = ErrorUnsupportedMediaType::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let r: HttpResponse = ErrorRangeNotSatisfiable::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);

        let r: HttpResponse = ErrorExpectationFailed::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::EXPECTATION_FAILED);

        let r: HttpResponse = ErrorImATeapot::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::IM_A_TEAPOT);

        let r: HttpResponse = ErrorMisdirectedRequest::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::MISDIRECTED_REQUEST);

        let r: HttpResponse = ErrorUnprocessableEntity::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let r: HttpResponse = ErrorLocked::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::LOCKED);

        let r: HttpResponse = ErrorFailedDependency::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::FAILED_DEPENDENCY);

        let r: HttpResponse = ErrorUpgradeRequired::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED);

        let r: HttpResponse = ErrorPreconditionRequired::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::PRECONDITION_REQUIRED);

        let r: HttpResponse = ErrorTooManyRequests::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

        let r: HttpResponse =
            ErrorRequestHeaderFieldsTooLarge::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);

        let r: HttpResponse =
            ErrorUnavailableForLegalReasons::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS);

        let r: HttpResponse = ErrorInternalServerError::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let r: HttpResponse = ErrorNotImplemented::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::NOT_IMPLEMENTED);

        let r: HttpResponse = ErrorBadGateway::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::BAD_GATEWAY);

        let r: HttpResponse = ErrorServiceUnavailable::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);

        let r: HttpResponse = ErrorGatewayTimeout::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::GATEWAY_TIMEOUT);

        let r: HttpResponse = ErrorHttpVersionNotSupported::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::HTTP_VERSION_NOT_SUPPORTED);

        let r: HttpResponse = ErrorVariantAlsoNegotiates::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::VARIANT_ALSO_NEGOTIATES);

        let r: HttpResponse = ErrorInsufficientStorage::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::INSUFFICIENT_STORAGE);

        let r: HttpResponse = ErrorLoopDetected::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::LOOP_DETECTED);

        let r: HttpResponse = ErrorNotExtended::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::NOT_EXTENDED);

        let r: HttpResponse =
            ErrorNetworkAuthenticationRequired::<_, DefaultError>("err").into();
        assert_eq!(r.status(), StatusCode::NETWORK_AUTHENTICATION_REQUIRED);
    }
}
