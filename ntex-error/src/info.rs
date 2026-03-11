use std::{fmt, fmt::Write, sync::Arc};

use ntex_bytes::{ByteString, BytesMut};

use crate::{Backtrace, Error, ErrorDiagnostic, ErrorKind, ResultType, repr::ErrorRepr};

trait ErrorInfo: fmt::Debug + 'static {
    fn tp(&self) -> ResultType;

    fn service(&self) -> Option<&'static str>;

    fn signature(&self) -> &'static str;

    fn description(&self) -> ByteString;

    fn backtrace(&self) -> Option<&Backtrace>;
}

impl<E> ErrorInfo for ErrorRepr<E>
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

    fn description(&self) -> ByteString {
        let mut buf = BytesMut::with_capacity(64);
        let _ = write!(buf, "{}", self.kind());
        ByteString::try_from(buf).unwrap()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        ErrorDiagnostic::backtrace(self)
    }
}

#[derive(Debug, Clone)]
pub struct ErrorInformation {
    inner: Arc<dyn ErrorInfo>,
}

impl ErrorInformation {
    pub fn tp(&self) -> ResultType {
        self.inner.tp()
    }

    pub fn service(&self) -> Option<&'static str> {
        self.inner.service()
    }

    pub fn signature(&self) -> &'static str {
        self.inner.signature()
    }

    pub fn description(&self) -> ByteString {
        self.inner.description()
    }

    pub fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }
}

impl<E> From<Error<E>> for ErrorInformation
where
    E: ErrorDiagnostic,
{
    fn from(err: Error<E>) -> Self {
        Self { inner: err.inner }
    }
}

impl<E> From<&Error<E>> for ErrorInformation
where
    E: ErrorDiagnostic,
{
    fn from(err: &Error<E>) -> Self {
        Self {
            inner: err.inner.clone(),
        }
    }
}
