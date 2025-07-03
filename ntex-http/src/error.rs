use std::{error, fmt, result};

pub use http::header::InvalidHeaderName;
pub use http::method::InvalidMethod;
pub use http::status::InvalidStatusCode;
pub use http::uri::InvalidUri;

pub use crate::value::{InvalidHeaderValue, ToStrError};

use http::header;
use http::method;
use http::status;
use http::uri;

/// A generic "error" for HTTP connections
///
/// This error type is less specific than the error returned from other
/// functions in this crate, but all other errors can be converted to this
/// error. Consumers of this crate can typically consume and work with this form
/// of error for conversions with the `?` operator.
pub struct Error {
    inner: ErrorKind,
}

/// A `Result` typedef to use with the `http::Error` type
pub type Result<T> = result::Result<T, Error>;

enum ErrorKind {
    StatusCode(status::InvalidStatusCode),
    Method(method::InvalidMethod),
    Uri(uri::InvalidUri),
    UriParts(uri::InvalidUriParts),
    HeaderName(header::InvalidHeaderName),
    HeaderValue(InvalidHeaderValue),
    Http(http::Error),
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
            Http(ref e) => e,
        }
    }
}

impl error::Error for Error {
    // Return any available cause from the inner error. Note the inner error is
    // not itself the cause.
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.get_ref().source()
    }
}

impl From<status::InvalidStatusCode> for Error {
    fn from(err: status::InvalidStatusCode) -> Error {
        Error {
            inner: ErrorKind::StatusCode(err),
        }
    }
}

impl From<method::InvalidMethod> for Error {
    fn from(err: method::InvalidMethod) -> Error {
        Error {
            inner: ErrorKind::Method(err),
        }
    }
}

impl From<uri::InvalidUri> for Error {
    fn from(err: uri::InvalidUri) -> Error {
        Error {
            inner: ErrorKind::Uri(err),
        }
    }
}

impl From<uri::InvalidUriParts> for Error {
    fn from(err: uri::InvalidUriParts) -> Error {
        Error {
            inner: ErrorKind::UriParts(err),
        }
    }
}

impl From<header::InvalidHeaderName> for Error {
    fn from(err: header::InvalidHeaderName) -> Error {
        Error {
            inner: ErrorKind::HeaderName(err),
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
        Error {
            inner: ErrorKind::Http(err),
        }
    }
}

impl From<std::convert::Infallible> for Error {
    fn from(err: std::convert::Infallible) -> Error {
        match err {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as StdError;

    #[test]
    fn inner_http_error() {
        let e = method::Method::from_bytes(b"").unwrap_err();
        let err: Error = http::Error::from(e).into();
        let ie = err.get_ref();
        assert!(ie.is::<http::Error>());
    }

    #[test]
    fn inner_error_is_invalid_status_code() {
        let e = status::StatusCode::from_u16(6666).unwrap_err();
        let err: Error = e.into();
        let ie = err.get_ref();
        assert!(!ie.is::<header::InvalidHeaderValue>());
        assert!(ie.is::<status::InvalidStatusCode>());
        ie.downcast_ref::<status::InvalidStatusCode>().unwrap();

        assert!(err.source().is_none());
        assert!(!err.is::<InvalidHeaderValue>());
        assert!(err.is::<status::InvalidStatusCode>());

        let s = format!("{err:?}");
        assert!(s.starts_with("ntex_http::Error"));
    }

    #[test]
    fn inner_error_is_invalid_method() {
        let e = method::Method::from_bytes(b"").unwrap_err();
        let err: Error = e.into();
        let ie = err.get_ref();
        assert!(ie.is::<method::InvalidMethod>());
        ie.downcast_ref::<method::InvalidMethod>().unwrap();

        assert!(err.source().is_none());
        assert!(err.is::<method::InvalidMethod>());

        let s = format!("{err:?}");
        assert!(s.starts_with("ntex_http::Error"));
    }
}
