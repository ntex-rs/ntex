use std::{error, fmt, result};

pub use crate::value::{InvalidHeaderValue, ToStrError};

#[derive(Clone)]
/// A generic "error" for HTTP connections
///
/// This error type is less specific than the error returned from other
/// functions in this crate, but all other errors can be converted to this
/// error. Consumers of this crate can typically consume and work with this form
/// of error for conversions with the `?` operator.
pub struct Error {
    inner: ErrorKind,
}

#[derive(thiserror::Error, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[error("Invalid URI")]
/// An error resulting from a failed attempt to construct a URI.
pub struct InvalidUri {
    _priv: (),
}

#[derive(thiserror::Error, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[error("Invalid HTTP header name")]
/// A possible error when converting a `HeaderName` from another type.
pub struct InvalidHeaderName {
    _priv: (),
}

#[derive(thiserror::Error, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[error("Invalid status code")]
/// A possible error value when converting a `StatusCode` from a `u16` or `&str`.
///
/// This error indicates that the supplied input was not a valid number, was less
/// than 100, or was greater than 999.
pub struct InvalidStatusCode {
    _priv: (),
}

#[derive(thiserror::Error, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[error("Invalid HTTP method")]
/// A possible error value when converting `Method` from bytes.
pub struct InvalidMethod {
    _priv: (),
}

/// A `Result` typedef to use with the `http::Error` type
pub type Result<T> = result::Result<T, Error>;

#[derive(Clone)]
enum ErrorKind {
    StatusCode(InvalidStatusCode),
    Method(InvalidMethod),
    Uri(InvalidUri),
    UriParts(InvalidUri),
    HeaderName(InvalidHeaderName),
    HeaderValue(InvalidHeaderValue),
    Http,
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ntex_http::Error")
            // Skip the noise of the ErrorKind enum
            .field(&self.get_ref())
            .finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.get_ref(), f)
    }
}

impl Error {
    /// Return true if the underlying error has the same type as T.
    pub fn is<T: error::Error + 'static>(&self) -> bool {
        self.get_ref().is::<T>()
    }

    /// Return a reference to the lower level, inner error.
    pub fn get_ref(&self) -> &(dyn error::Error + 'static) {
        use self::ErrorKind::*;

        match self.inner {
            StatusCode(ref e) => e,
            Method(ref e) => e,
            Uri(ref e) => e,
            UriParts(ref e) => e,
            HeaderName(ref e) => e,
            HeaderValue(ref e) => e,
            Http => &DEFAULT_ERR,
        }
    }
}

#[derive(thiserror::Error, Copy, Clone, Debug)]
#[error("{_0}")]
struct ErrorMessage(&'static str);

const DEFAULT_ERR: ErrorMessage = ErrorMessage("http error");

impl error::Error for Error {
    // Return any available cause from the inner error. Note the inner error is
    // not itself the cause.
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.get_ref().source()
    }
}

impl From<http::status::InvalidStatusCode> for Error {
    fn from(_: http::status::InvalidStatusCode) -> Error {
        Error {
            inner: ErrorKind::StatusCode(InvalidStatusCode { _priv: () }),
        }
    }
}

impl From<http::method::InvalidMethod> for Error {
    fn from(_: http::method::InvalidMethod) -> Error {
        Error {
            inner: ErrorKind::Method(InvalidMethod { _priv: () }),
        }
    }
}

impl From<http::uri::InvalidUri> for Error {
    fn from(_: http::uri::InvalidUri) -> Error {
        Error {
            inner: ErrorKind::Uri(InvalidUri { _priv: () }),
        }
    }
}

impl From<http::uri::InvalidUriParts> for Error {
    fn from(_: http::uri::InvalidUriParts) -> Error {
        Error {
            inner: ErrorKind::UriParts(InvalidUri { _priv: () }),
        }
    }
}

impl From<http::header::InvalidHeaderName> for Error {
    fn from(_: http::header::InvalidHeaderName) -> Error {
        Error {
            inner: ErrorKind::HeaderName(InvalidHeaderName { _priv: () }),
        }
    }
}

impl From<InvalidHeaderValue> for Error {
    fn from(err: InvalidHeaderValue) -> Error {
        Error {
            inner: ErrorKind::HeaderValue(err),
        }
    }
}

impl From<http::Error> for Error {
    fn from(err: http::Error) -> Error {
        let inner = if err.is::<http::status::InvalidStatusCode>() {
            ErrorKind::StatusCode(InvalidStatusCode { _priv: () })
        } else if err.is::<http::method::InvalidMethod>() {
            ErrorKind::Method(InvalidMethod { _priv: () })
        } else if err.is::<http::uri::InvalidUri>() {
            ErrorKind::Uri(InvalidUri { _priv: () })
        } else if err.is::<http::header::InvalidHeaderName>() {
            ErrorKind::HeaderName(InvalidHeaderName { _priv: () })
        } else if err.is::<http::header::InvalidHeaderValue>() {
            ErrorKind::HeaderValue(InvalidHeaderValue::default())
        } else {
            ErrorKind::Http
        };
        Error { inner }
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(err: std::convert::Infallible) -> Error {
        match err {}
    }
}

impl From<http::status::InvalidStatusCode> for InvalidStatusCode {
    fn from(_: http::status::InvalidStatusCode) -> InvalidStatusCode {
        InvalidStatusCode { _priv: () }
    }
}

impl From<http::method::InvalidMethod> for InvalidMethod {
    fn from(_: http::method::InvalidMethod) -> InvalidMethod {
        InvalidMethod { _priv: () }
    }
}

impl From<http::uri::InvalidUri> for InvalidUri {
    fn from(_: http::uri::InvalidUri) -> InvalidUri {
        InvalidUri { _priv: () }
    }
}

impl From<http::header::InvalidHeaderName> for InvalidHeaderName {
    fn from(_: http::header::InvalidHeaderName) -> InvalidHeaderName {
        InvalidHeaderName { _priv: () }
    }
}

impl From<http::header::InvalidHeaderValue> for InvalidHeaderValue {
    fn from(_: http::header::InvalidHeaderValue) -> InvalidHeaderValue {
        InvalidHeaderValue::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    #[test]
    fn inner_http_error() {
        let e = http::method::Method::from_bytes(b"").unwrap_err();
        let err: Error = http::Error::from(e).into();
        let ie = err.get_ref();
        assert!(!ie.is::<http::Error>());
    }

    #[test]
    fn inner_error_is_invalid_status_code() {
        let e = http::status::StatusCode::from_u16(6666).unwrap_err();
        let err: Error = e.into();
        let ie = err.get_ref();
        assert!(!ie.is::<InvalidHeaderValue>());
        assert!(ie.is::<InvalidStatusCode>());
        ie.downcast_ref::<InvalidStatusCode>().unwrap();

        assert!(err.source().is_none());
        assert!(!err.is::<InvalidHeaderValue>());
        assert!(err.is::<InvalidStatusCode>());

        let s = format!("{err:?}");
        assert!(s.starts_with("ntex_http::Error"));
    }

    #[test]
    fn inner_error_is_invalid_method() {
        let e = http::method::Method::from_bytes(b"").unwrap_err();
        let err: Error = e.into();
        let ie = err.get_ref();
        assert!(ie.is::<InvalidMethod>());
        ie.downcast_ref::<InvalidMethod>().unwrap();

        assert!(err.source().is_none());
        assert!(err.is::<InvalidMethod>());

        let s = format!("{err:?}");
        assert!(s.starts_with("ntex_http::Error"));
    }
}
