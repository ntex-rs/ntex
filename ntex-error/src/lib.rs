//! Error management.
#![deny(clippy::pedantic)]
#![allow(clippy::must_use_candidate, clippy::missing_panics_doc)]
use std::{error, fmt, ops, panic::Location, sync::Arc};

mod bt;
mod chain;
mod info;
mod repr;

pub use crate::bt::{Backtrace, set_backtrace_start, set_backtrace_start_alt};
pub use crate::chain::ErrorChain;
pub use crate::info::ErrorInformation;

use self::repr::ErrorRepr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorType {
    Client,
    Service,
}

impl ErrorType {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ErrorType::Client => "ClientError",
            ErrorType::Service => "ServiceError",
        }
    }
}

pub trait ErrorKind: fmt::Display + fmt::Debug + 'static {
    /// Defines type of the error
    fn error_type(&self) -> ErrorType;
}

impl ErrorKind for ErrorType {
    fn error_type(&self) -> ErrorType {
        *self
    }
}

pub trait ErrorDiagnostic: error::Error + 'static {
    type Kind: ErrorKind;

    /// Provides specific kind of the error
    fn kind(&self) -> Self::Kind;

    /// Provides a string to identify responsible service
    fn service(&self) -> Option<&'static str> {
        None
    }

    /// Check if error is service related
    fn is_service(&self) -> bool {
        self.kind().error_type() == ErrorType::Service
    }

    /// Provides a string to identify specific kind of the error
    fn signature(&self) -> &'static str {
        self.kind().error_type().as_str()
    }

    /// Provides error call location
    fn backtrace(&self) -> Option<&Backtrace> {
        None
    }

    #[track_caller]
    fn chain(self) -> ErrorChain<Self::Kind>
    where
        Self: Sized,
    {
        ErrorChain::new(self)
    }
}

pub struct Error<E: ErrorDiagnostic> {
    pub(crate) inner: Arc<ErrorRepr<E, E::Kind>>,
}

impl<E: ErrorDiagnostic> Error<E> {
    #[track_caller]
    pub fn new<T>(error: T, service: &'static str) -> Self
    where
        E: ErrorDiagnostic,
        E: From<T>,
    {
        Self {
            inner: Arc::new(ErrorRepr::new(
                E::from(error),
                Some(service),
                Location::caller(),
            )),
        }
    }

    #[must_use]
    /// Set response service
    pub fn set_service(mut self, name: &'static str) -> Self
    where
        E: Clone,
    {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.service = Some(name);
            self
        } else {
            Error {
                inner: Arc::new(ErrorRepr::new2(
                    self.inner.error.clone(),
                    Some(name),
                    self.inner.backtrace.clone(),
                )),
            }
        }
    }

    /// Map inner error to new error
    ///
    /// Keep same `service` and `location`
    pub fn map<U, F>(self, f: F) -> Error<U>
    where
        E: Clone,
        F: FnOnce(E) -> U,
        U: ErrorDiagnostic,
    {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => Error {
                inner: Arc::new(ErrorRepr::new2(
                    f(inner.error),
                    inner.service,
                    inner.backtrace,
                )),
            },
            Err(inner) => Error {
                inner: Arc::new(ErrorRepr::new2(
                    f(inner.error.clone()),
                    inner.service,
                    inner.backtrace.clone(),
                )),
            },
        }
    }

    /// Get inner error value
    pub fn into_error(self) -> E
    where
        E: Clone,
    {
        Arc::try_unwrap(self.inner)
            .map_or_else(|inner| inner.error.clone(), |inner| inner.error)
    }
}

impl<E: ErrorDiagnostic + Clone> Clone for Error<E> {
    fn clone(&self) -> Error<E> {
        Error {
            inner: self.inner.clone(),
        }
    }
}

impl<E: ErrorDiagnostic> From<E> for Error<E> {
    #[track_caller]
    fn from(error: E) -> Self {
        Self {
            inner: Arc::new(ErrorRepr::new(error, None, Location::caller())),
        }
    }
}

impl<E: ErrorDiagnostic> Eq for Error<E> where E: Eq {}

impl<E: ErrorDiagnostic> PartialEq for Error<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner.error.eq(&other.inner.error) && self.inner.service == other.inner.service
    }
}

impl<E: ErrorDiagnostic> PartialEq<E> for Error<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &E) -> bool {
        self.inner.error.eq(other)
    }
}

impl<E: ErrorDiagnostic> ops::Deref for Error<E> {
    type Target = E;

    fn deref(&self) -> &E {
        &self.inner.error
    }
}

impl<E: ErrorDiagnostic> error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.inner.error)
    }
}

impl<E: ErrorDiagnostic> ErrorDiagnostic for Error<E> {
    type Kind = E::Kind;

    fn kind(&self) -> Self::Kind {
        self.inner.kind()
    }

    fn service(&self) -> Option<&'static str> {
        self.inner.service()
    }

    fn signature(&self) -> &'static str {
        self.inner.signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }
}

impl<E: ErrorDiagnostic> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner.error, f)
    }
}

impl<E: ErrorDiagnostic> fmt::Debug for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("error", &self.inner.error)
            .field("service", &self.inner.service)
            .field("backtrace", &self.inner.backtrace)
            .finish()
    }
}

impl fmt::Display for ErrorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorType::Client => write!(f, "ClientError"),
            ErrorType::Service => write!(f, "ServiceError"),
        }
    }
}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
    use std::{error::Error as StdError, mem};

    use super::*;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
    enum TestKind {
        #[error("Connect")]
        Connect,
        #[error("Disconnect")]
        Disconnect,
        #[error("ServiceError")]
        ServiceError,
    }

    impl ErrorKind for TestKind {
        fn error_type(&self) -> ErrorType {
            match self {
                TestKind::Connect | TestKind::Disconnect => ErrorType::Client,
                TestKind::ServiceError => ErrorType::Service,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    enum TestError {
        #[error("Connect err: {0}")]
        Connect(&'static str),
        #[error("Disconnect")]
        Disconnect,
        #[error("InternalServiceError")]
        Service(&'static str),
    }

    impl ErrorDiagnostic for TestError {
        type Kind = TestKind;

        fn kind(&self) -> Self::Kind {
            match self {
                TestError::Connect(_) => TestKind::Connect,
                TestError::Disconnect => TestKind::Disconnect,
                TestError::Service(_) => TestKind::ServiceError,
            }
        }

        fn service(&self) -> Option<&'static str> {
            Some("test")
        }

        fn signature(&self) -> &'static str {
            match self {
                TestError::Connect(_) => "Client-Connect",
                TestError::Disconnect => "Client-Disconnect",
                TestError::Service(_) => "Service-Internal",
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    #[error("TestError2")]
    struct TestError2;
    impl ErrorDiagnostic for TestError2 {
        type Kind = ErrorType;

        fn kind(&self) -> Self::Kind {
            ErrorType::Client
        }
    }

    #[ntex::test]
    async fn test_error() {
        let err: Error<TestError> = TestError::Service("409 Error").into();
        let err = err.clone();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!((*err).kind(), TestKind::ServiceError);
        assert_eq!(err.to_string(), "InternalServiceError");
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.signature(), "Service-Internal");
        assert_eq!(
            err,
            Into::<Error<TestError>>::into(TestError::Service("409 Error"))
        );
        assert!(err.backtrace().is_some());
        assert!(err.is_service());
        assert!(
            format!("{:?}", err.source()).contains("Service(\"409 Error\")"),
            "{:?}",
            err.source().unwrap()
        );

        let err = err.set_service("SVC");
        assert_eq!(err.service(), Some("SVC"));

        let err2: Error<TestError> = Error::new(TestError::Service("409 Error"), "TEST");
        assert!(err != err2);
        assert_eq!(err, TestError::Service("409 Error"));

        let err2 = err2.set_service("SVC");
        assert_eq!(err, err2);
        let err2 = err2.map(|_| TestError::Disconnect);
        assert!(err != err2);

        assert_eq!(
            TestError::Connect("").kind().error_type(),
            ErrorType::Client
        );
        assert_eq!(TestError::Disconnect.kind().error_type(), ErrorType::Client);
        assert_eq!(
            TestError::Service("").kind().error_type(),
            ErrorType::Service
        );
        assert_eq!(TestError::Connect("").to_string(), "Connect err: ");
        assert_eq!(TestError::Disconnect.to_string(), "Disconnect");
        assert_eq!(TestError::Disconnect.service(), Some("test"));
        assert!(TestError::Disconnect.backtrace().is_none());

        assert_eq!(ErrorType::Client.as_str(), "ClientError");
        assert_eq!(ErrorType::Service.as_str(), "ServiceError");
        assert_eq!(ErrorType::Client.error_type(), ErrorType::Client);
        assert_eq!(ErrorType::Service.error_type(), ErrorType::Service);
        assert_eq!(ErrorType::Client.to_string(), "ClientError");
        assert_eq!(ErrorType::Service.to_string(), "ServiceError");

        assert_eq!(TestKind::Connect.to_string(), "Connect");
        assert_eq!(TestError::Connect("").signature(), "Client-Connect");
        assert_eq!(TestKind::Disconnect.to_string(), "Disconnect");
        assert_eq!(TestError::Disconnect.signature(), "Client-Disconnect");
        assert_eq!(TestKind::ServiceError.to_string(), "ServiceError");
        assert_eq!(TestError::Service("").signature(), "Service-Internal");

        let err = err.into_error().chain();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!(err.kind(), TestError::Service("409 Error").kind());
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(format!("{err:?}").contains("Service(\"409 Error\")"));
        assert!(
            format!("{:?}", err.source()).contains("Service(\"409 Error\")"),
            "{:?}",
            err.source().unwrap()
        );

        let err: Error<TestError> = TestError::Service("404 Error").into();
        assert!(
            format!("{}", err.backtrace().unwrap())
                .contains("ntex_error::tests::test_error"),
            "{}",
            err.backtrace().unwrap()
        );
        assert!(
            err.backtrace()
                .unwrap()
                .repr()
                .contains("ntex_error::tests::test_error"),
            "{}",
            err.backtrace().unwrap()
        );

        let err: ErrorChain<TestKind> = err.into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!(err.kind(), TestError::Service("404 Error").kind());
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.signature(), "Service-Internal");
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(err.backtrace().is_some());
        assert!(format!("{err:?}").contains("Service(\"404 Error\")"));

        assert_eq!(24, mem::size_of::<TestError>());
        assert_eq!(8, mem::size_of::<Error<TestError>>());

        assert_eq!(TestError2.service(), None);
        assert_eq!(TestError2.signature(), "ClientError");

        // ErrorInformation
        let err: Error<TestError> = TestError::Service("409 Error").into();
        let err: ErrorInformation = err.set_service("SVC").into();
        assert_eq!(err.error_type(), ErrorType::Service);
        assert_eq!(err.error_signature(), "ServiceError");
        assert_eq!(err.service(), Some("SVC"));
        assert_eq!(err.signature(), "Service-Internal");
        assert!(err.backtrace().is_some());
    }
}
