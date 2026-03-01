//! Error management.
use std::{error, fmt, fmt::Write, marker::PhantomData, ops, panic::Location, sync::Arc};

use ntex_bytes::{ByteString, BytesMut};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ErrorType {
    #[error("Success")]
    Success,
    #[error("ClientError")]
    ClientError,
    #[error("ServiceError")]
    ServiceError,
}

impl ErrorType {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ErrorType::Success => "Success",
            ErrorType::ClientError => "ClientError",
            ErrorType::ServiceError => "ServiceError",
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

    /// Provides a string to identify specific kind of the error
    fn signature(&self) -> &'static str {
        self.kind().error_type().as_str()
    }

    #[inline]
    /// Provides error call location
    fn location(&self) -> Option<&'static Location<'static>> {
        None
    }

    #[inline]
    /// Traverse error chain
    fn traverse(&self, f: &mut dyn FnMut(&dyn ErrorInfo))
    where
        Self: Sized,
    {
        f(&ErrorInfoWrapper { inner: self });
    }

    #[inline]
    #[track_caller]
    fn chain(self) -> ErrorChain<Self::Kind>
    where
        Self: Sized,
    {
        ErrorChain::new(self)
    }
}

pub trait ErrorInfo {
    fn error_type(&self) -> ErrorType;

    fn error_signature(&self) -> ByteString;

    fn signature(&self) -> &'static str;

    fn description(&self) -> ByteString;

    fn location(&self) -> Option<&'static Location<'static>>;
}

trait Traversable<K>: ErrorDiagnostic<Kind = K> {
    fn traverse(&self, f: &mut dyn FnMut(&dyn ErrorInfo));
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{error}")]
pub struct Error<E> {
    #[source]
    error: E,
    location: &'static Location<'static>,
}

impl<E> Error<E> {
    pub const fn new(error: E, location: &'static Location<'static>) -> Self {
        Self { error, location }
    }

    /// Get inner error value
    pub fn into_error(self) -> E {
        self.error
    }

    pub fn map<U, F>(self, f: F) -> Error<U>
    where
        F: FnOnce(E) -> U,
    {
        Error {
            error: f(self.error),
            location: self.location,
        }
    }
}

impl<E> From<E> for Error<E> {
    #[track_caller]
    #[inline]
    fn from(error: E) -> Self {
        Self::new(error, Location::caller())
    }
}

impl<E> PartialEq for Error<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.error.eq(&other.error)
    }
}

impl<E> Eq for Error<E> where E: Eq {}

impl<E> ops::Deref for Error<E> {
    type Target = E;

    fn deref(&self) -> &E {
        &self.error
    }
}

impl<E> ErrorDiagnostic for Error<E>
where
    E: ErrorDiagnostic,
{
    type Kind = E::Kind;

    #[inline]
    fn kind(&self) -> Self::Kind {
        self.error.kind()
    }

    #[inline]
    fn signature(&self) -> &'static str {
        self.error.signature()
    }

    #[inline]
    fn location(&self) -> Option<&'static Location<'static>> {
        Some(self.location)
    }

    #[inline]
    /// Traverse error chain
    fn traverse(&self, f: &mut dyn FnMut(&dyn ErrorInfo)) {
        f(&ErrorInfoWrapper { inner: self });
        self.error.traverse(f);
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{error}")]
pub struct ErrorChain<K: ErrorKind> {
    #[source]
    error: Arc<dyn Traversable<K>>,
}

impl<K: ErrorKind> ErrorChain<K> {
    #[track_caller]
    pub fn new<E>(error: E) -> Self
    where
        E: ErrorDiagnostic + Sized,
        E::Kind: Into<K>,
    {
        Self {
            error: Arc::new(ErrorChainWrapper {
                error,
                location: Location::caller(),
                _k: PhantomData,
            }),
        }
    }
}

impl<E, K> From<Error<E>> for ErrorChain<K>
where
    E: ErrorDiagnostic + Sized,
    E::Kind: Into<K>,
    K: ErrorKind,
{
    fn from(err: Error<E>) -> Self {
        Self {
            error: Arc::new(ErrorChainWrapper {
                error: err.error,
                location: err.location,
                _k: PhantomData,
            }),
        }
    }
}

impl<K> ErrorDiagnostic for ErrorChain<K>
where
    K: ErrorKind,
{
    type Kind = K;

    #[inline]
    fn kind(&self) -> Self::Kind {
        self.error.kind()
    }

    #[inline]
    fn signature(&self) -> &'static str {
        self.error.signature()
    }

    #[inline]
    fn location(&self) -> Option<&'static Location<'static>> {
        self.error.location()
    }

    #[inline]
    fn traverse(&self, f: &mut dyn FnMut(&dyn ErrorInfo)) {
        Traversable::traverse(self.error.as_ref(), f);
    }
}

#[derive(thiserror::Error)]
#[error("{error}")]
struct ErrorChainWrapper<E: Sized, K> {
    #[source]
    error: E,
    location: &'static Location<'static>,
    _k: PhantomData<K>,
}

impl<E, K> Traversable<K> for ErrorChainWrapper<E, K>
where
    E: ErrorDiagnostic,
    E::Kind: Into<K>,
    K: ErrorKind,
{
    fn traverse(&self, f: &mut dyn FnMut(&dyn ErrorInfo)) {
        f(&ErrorInfoWrapper { inner: self });
    }
}

impl<E, K> ErrorDiagnostic for ErrorChainWrapper<E, K>
where
    E: ErrorDiagnostic,
    E::Kind: Into<K>,
    K: ErrorKind,
{
    type Kind = K;

    #[inline]
    fn kind(&self) -> Self::Kind {
        self.error.kind().into()
    }

    #[inline]
    fn signature(&self) -> &'static str {
        self.error.signature()
    }

    #[inline]
    fn location(&self) -> Option<&'static Location<'static>> {
        Some(self.location)
    }

    #[inline]
    fn traverse(&self, f: &mut dyn FnMut(&dyn ErrorInfo)) {
        f(&ErrorInfoWrapper { inner: self });
        self.error.traverse(f);
    }
}

impl<E, K> fmt::Debug for ErrorChainWrapper<E, K>
where
    E: error::Error,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.error, f)
    }
}

struct ErrorInfoWrapper<'a, E: ErrorDiagnostic> {
    inner: &'a E,
}

impl<'a, E: ErrorDiagnostic> ErrorInfo for ErrorInfoWrapper<'a, E> {
    #[inline]
    fn error_type(&self) -> ErrorType {
        self.inner.kind().error_type()
    }

    #[inline]
    fn error_signature(&self) -> ByteString {
        let mut buf = BytesMut::new();
        let _ = write!(&mut buf, "{}", self.inner.kind());
        ByteString::try_from(buf).unwrap()
    }

    #[inline]
    fn signature(&self) -> &'static str {
        self.inner.signature()
    }

    #[inline]
    fn description(&self) -> ByteString {
        let mut buf = BytesMut::new();
        let _ = write!(&mut buf, "{}", self.inner);
        ByteString::try_from(buf).unwrap()
    }

    #[inline]
    fn location(&self) -> Option<&'static Location<'static>> {
        self.inner.location()
    }
}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
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
                TestKind::Connect => ErrorType::ClientError,
                TestKind::Disconnect => ErrorType::ClientError,
                TestKind::ServiceError => ErrorType::ServiceError,
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

        fn signature(&self) -> &'static str {
            match self {
                TestError::Connect(_) => "Client-Connect",
                TestError::Disconnect => "Client-Disconnect",
                TestError::Service(_) => "Service-Internal",
            }
        }
    }

    #[test]
    fn test_error() {
        let err: Error<TestError> = TestError::Service("409 Error").into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!((*err).kind(), TestKind::ServiceError);
        assert_eq!(err.to_string(), "InternalServiceError");
        assert_eq!(
            err,
            Into::<Error<TestError>>::into(TestError::Service("409 Error"))
        );
        assert!(err.location().is_some());

        err.traverse(&mut |info| {
            let _ = info.location();
            assert_eq!(info.error_type(), ErrorType::ServiceError);
            assert_eq!(info.error_signature(), "ServiceError");
            assert_eq!(info.signature(), "Service-Internal");
            assert_eq!(info.description(), "InternalServiceError");
        });

        assert_eq!(
            TestError::Connect("").kind().error_type(),
            ErrorType::ClientError
        );
        assert_eq!(
            TestError::Disconnect.kind().error_type(),
            ErrorType::ClientError
        );
        assert_eq!(
            TestError::Service("").kind().error_type(),
            ErrorType::ServiceError
        );
        assert_eq!(TestError::Connect("").to_string(), "Connect err: ");
        assert_eq!(TestError::Disconnect.to_string(), "Disconnect");
        assert!(TestError::Disconnect.location().is_none());

        TestError::Connect("").traverse(&mut |info| {
            let _ = info.location();
            assert_eq!(info.error_type(), ErrorType::ClientError);
            assert_eq!(info.error_signature(), "Connect");
            assert_eq!(info.signature(), "Client-Connect");
            assert_eq!(info.description(), "Connect err: ");
        });

        assert_eq!(ErrorType::Success.as_str(), "Success");
        assert_eq!(ErrorType::ClientError.as_str(), "ClientError");
        assert_eq!(ErrorType::ServiceError.as_str(), "ServiceError");
        assert_eq!(ErrorType::Success.error_type(), ErrorType::Success);
        assert_eq!(ErrorType::ClientError.error_type(), ErrorType::ClientError);
        assert_eq!(
            ErrorType::ServiceError.error_type(),
            ErrorType::ServiceError
        );
        assert_eq!(ErrorType::Success.to_string(), "Success");
        assert_eq!(ErrorType::ClientError.to_string(), "ClientError");
        assert_eq!(ErrorType::ServiceError.to_string(), "ServiceError");

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
        assert!(format!("{:?}", err).contains("Service(\"409 Error\")"));

        let err: Error<TestError> = TestError::Service("404 Error").into();
        let err: ErrorChain<TestKind> = err.into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!(err.kind(), TestError::Service("404 Error").kind());
        assert_eq!(err.signature(), "Service-Internal");
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(err.location().is_some());
        assert!(format!("{:?}", err).contains("Service(\"404 Error\")"));

        err.traverse(&mut |info| {
            assert!(info.location().is_some());
            assert_eq!(info.error_type(), ErrorType::ServiceError);
            assert_eq!(info.error_signature(), "ServiceError");
            assert_eq!(info.signature(), "Service-Internal");
            assert_eq!(info.description(), "InternalServiceError");
        });
    }
}
