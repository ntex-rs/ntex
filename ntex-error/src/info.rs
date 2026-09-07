use ntex_bytes::Bytes;
use std::{any::Any, any::TypeId, error, fmt, sync::Arc};

use crate::{AsError, Backtrace, Error, ErrorDiagnostic, IntoFailure, repr::ErrorRepr};

trait ErrorInfo: fmt::Display + fmt::Debug + 'static {
    fn tag(&self) -> Option<&crate::Bytes>;

    fn service(&self) -> Option<&'static str>;

    fn signature(&self) -> &'static str;

    fn backtrace(&self) -> Option<&Backtrace>;

    fn source(&self) -> Option<&(dyn error::Error + 'static)>;

    fn get_item(&self, id: &TypeId) -> Option<&(dyn Any + Send + Sync)>;
}

impl<E> ErrorInfo for ErrorRepr<E>
where
    E: ErrorDiagnostic,
{
    fn tag(&self) -> Option<&crate::Bytes> {
        ErrorDiagnostic::tag(self)
    }

    fn service(&self) -> Option<&'static str> {
        ErrorDiagnostic::service(self)
    }

    fn signature(&self) -> &'static str {
        self.error.signature()
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
pub struct Failure {
    inner: FailureDiagnostic,
}

pub struct FailureDiagnostic(Arc<dyn ErrorInfo>);

impl Failure {
    /// Returns an optional tag associated with this error.
    pub fn tag(&self) -> Option<&crate::Bytes> {
        self.inner.0.tag()
    }

    /// Returns the name of the responsible service, if applicable.
    pub fn service(&self) -> Option<&'static str> {
        self.inner.0.service()
    }

    /// Returns a stable identifier for the specific error classification.
    pub fn signature(&self) -> &'static str {
        self.inner.0.signature()
    }

    /// Returns a backtrace for debugging purposes, if available.
    pub fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.0.backtrace()
    }

    /// Returns a reference to a previously stored value of type `T` from this error.
    pub fn get_item<T: 'static>(&self) -> Option<&T> {
        self.inner
            .0
            .get_item(&TypeId::of::<T>())
            .and_then(|boxed| boxed.downcast_ref())
    }
}

impl<E> From<Error<E>> for Failure
where
    E: ErrorDiagnostic,
{
    fn from(err: Error<E>) -> Self {
        Self {
            inner: FailureDiagnostic(err.inner),
        }
    }
}

impl<E> From<&Error<E>> for Failure
where
    E: ErrorDiagnostic,
{
    fn from(err: &Error<E>) -> Self {
        Self {
            inner: FailureDiagnostic(err.inner.clone()),
        }
    }
}

impl IntoFailure for Failure {
    fn fail(self) -> Failure {
        self
    }
}

impl<E> IntoFailure for E
where
    E: ErrorDiagnostic + Into<Error<E>>,
{
    fn fail(self) -> Failure {
        Failure {
            inner: FailureDiagnostic(self.into().inner),
        }
    }
}

impl error::Error for Failure {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.inner.source()
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, f)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner, f)
    }
}

impl Clone for Failure {
    fn clone(&self) -> Self {
        Failure {
            inner: FailureDiagnostic(self.inner.0.clone()),
        }
    }
}

impl AsError for Failure {
    type Target = FailureDiagnostic;

    fn as_diag(&self) -> &FailureDiagnostic {
        &self.inner
    }
}

impl ErrorDiagnostic for FailureDiagnostic {
    fn signature(&self) -> &'static str {
        self.0.signature()
    }

    fn tag(&self) -> Option<&Bytes> {
        self.0.tag()
    }

    fn service(&self) -> Option<&'static str> {
        self.0.service()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.0.backtrace()
    }
}

impl error::Error for FailureDiagnostic {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.0.source()
    }
}

impl fmt::Debug for FailureDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for FailureDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
