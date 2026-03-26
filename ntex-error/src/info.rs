use std::{any::Any, any::TypeId, error, fmt, sync::Arc};

use crate::{Backtrace, Error, ErrorDiagnostic, ResultKind, ResultType, repr::ErrorRepr};

/// Helper type holding a result type and classification signature.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ErrorInfoType(ResultType, &'static str);

impl ResultKind for ErrorInfoType {
    fn tp(&self) -> ResultType {
        self.0
    }

    fn signature(&self) -> &'static str {
        self.1
    }
}

trait ErrorInformation: fmt::Display + fmt::Debug + 'static {
    fn tp(&self) -> ResultType;

    fn tag(&self) -> Option<&crate::Bytes>;

    fn service(&self) -> Option<&'static str>;

    fn signature(&self) -> &'static str;

    fn backtrace(&self) -> Option<&Backtrace>;

    fn source(&self) -> Option<&(dyn error::Error + 'static)>;

    fn get_item(&self, id: &TypeId) -> Option<&(dyn Any + Send + Sync)>;
}

impl<E> ErrorInformation for ErrorRepr<E>
where
    E: ErrorDiagnostic,
{
    fn tp(&self) -> ResultType {
        self.error.kind().tp()
    }

    fn tag(&self) -> Option<&crate::Bytes> {
        ErrorDiagnostic::tag(self)
    }

    fn service(&self) -> Option<&'static str> {
        ErrorDiagnostic::service(self)
    }

    fn signature(&self) -> &'static str {
        self.error.kind().signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        ErrorDiagnostic::backtrace(self)
    }

    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.error.source()
    }

    fn get_item(&self, id: &TypeId) -> Option<&(dyn Any + Send + Sync)> {
        self.ext.map.get(id).map(AsRef::as_ref)
    }
}

/// Type-erased container holding error information.
///
/// This allows storing and passing error metadata without exposing the concrete type.
#[derive(Clone)]
pub struct ErrorInfo {
    inner: Arc<dyn ErrorInformation>,
}

impl ErrorInfo {
    /// Returns the classification of the result (e.g. success, client error, service error).
    pub fn tp(&self) -> ResultType {
        self.inner.tp()
    }

    /// Returns an optional tag associated with this error.
    pub fn tag(&self) -> Option<&crate::Bytes> {
        self.inner.tag()
    }

    /// Returns the name of the responsible service, if applicable.
    pub fn service(&self) -> Option<&'static str> {
        self.inner.service()
    }

    /// Returns a stable identifier for the specific error classification.
    pub fn signature(&self) -> &'static str {
        self.inner.signature()
    }

    /// Returns a backtrace for debugging purposes, if available.
    pub fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }

    /// Returns a reference to a previously stored value of type `T` from this error.
    pub fn get_item<T: 'static>(&self) -> Option<&T> {
        self.inner
            .get_item(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }
}

impl<E> From<Error<E>> for ErrorInfo
where
    E: ErrorDiagnostic,
{
    fn from(err: Error<E>) -> Self {
        Self { inner: err.inner }
    }
}

impl<E> From<&Error<E>> for ErrorInfo
where
    E: ErrorDiagnostic,
{
    fn from(err: &Error<E>) -> Self {
        Self {
            inner: err.inner.clone(),
        }
    }
}

impl error::Error for ErrorInfo {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.inner.source()
    }
}

impl fmt::Debug for ErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, f)
    }
}

impl fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl ErrorDiagnostic for ErrorInfo {
    type Kind = ErrorInfoType;

    fn kind(&self) -> ErrorInfoType {
        ErrorInfoType(self.tp(), self.inner.signature())
    }

    fn service(&self) -> Option<&'static str> {
        self.inner.service()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }
}
