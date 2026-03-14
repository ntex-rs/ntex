use std::{error, fmt, sync::Arc};

use crate::{Backtrace, Error, ErrorDiagnostic, ResultKind, ResultType, repr::ErrorRepr};

trait ErrorInfoInner: fmt::Display + fmt::Debug + 'static {
    fn tp(&self) -> ResultType;

    fn service(&self) -> Option<&'static str>;

    fn signature(&self) -> &'static str;

    fn backtrace(&self) -> Option<&Backtrace>;

    fn source(&self) -> Option<&(dyn error::Error + 'static)>;
}

impl<E> ErrorInfoInner for ErrorRepr<E>
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
}

#[derive(Debug, Clone)]
pub struct ErrorInfo {
    inner: Arc<dyn ErrorInfoInner>,
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

impl fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner)
    }
}
