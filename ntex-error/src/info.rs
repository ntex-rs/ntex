use std::{any::Any, any::TypeId, error, fmt, sync::Arc};

use crate::{Backtrace, Error, ErrorDiagnostic, ResultKind, ResultType, repr::ErrorRepr};

trait ErrorInformation: fmt::Display + fmt::Debug + 'static {
    fn tp(&self) -> ResultType;

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
        self.kind().tp()
    }

    fn service(&self) -> Option<&'static str> {
        ErrorDiagnostic::service(self)
    }

    fn signature(&self) -> &'static str {
        self.kind().signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        ErrorDiagnostic::backtrace(self)
    }

    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.error)
    }

    fn get_item(&self, id: &TypeId) -> Option<&(dyn Any + Send + Sync)> {
        self.ext.map.get(id).map(AsRef::as_ref)
    }
}

#[derive(Clone)]
pub struct ErrorInfo {
    inner: Arc<dyn ErrorInformation>,
}

impl AsRef<dyn ErrorInformation> for ErrorInfo {
    fn as_ref(&self) -> &dyn ErrorInformation {
        self.inner.as_ref()
    }
}

impl ErrorInfo {
    pub fn tp(&self) -> ResultType {
        self.inner.tp()
    }

    pub fn service(&self) -> Option<&'static str> {
        self.inner.service()
    }

    pub fn signature(&self) -> &'static str {
        self.inner.signature()
    }

    pub fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }

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
