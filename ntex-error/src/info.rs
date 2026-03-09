use std::{fmt, fmt::Write, sync::Arc};

use ntex_bytes::{ByteString, BytesMut};

use crate::{Backtrace, Error, ErrorDiagnostic, ErrorKind, ErrorRepr, ErrorType};

trait ErrorInfo: fmt::Debug + 'static {
    fn error_type(&self) -> ErrorType;

    fn error_signature(&self) -> ByteString;

    fn service(&self) -> Option<&'static str>;

    fn signature(&self) -> &'static str;

    fn backtrace(&self) -> Option<&Backtrace>;
}

impl<E, K> ErrorInfo for ErrorRepr<E, K>
where
    E: ErrorDiagnostic,
    K: ErrorKind + From<E::Kind>,
{
    fn error_type(&self) -> ErrorType {
        self.kind().error_type()
    }

    fn error_signature(&self) -> ByteString {
        let mut buf = BytesMut::new();
        let _ = write!(&mut buf, "{}", self.kind());
        ByteString::try_from(buf).unwrap()
    }

    fn service(&self) -> Option<&'static str> {
        ErrorDiagnostic::service(self)
    }

    fn signature(&self) -> &'static str {
        ErrorDiagnostic::signature(self)
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
    pub fn error_type(&self) -> ErrorType {
        self.inner.error_type()
    }

    pub fn error_signature(&self) -> ByteString {
        self.inner.error_signature()
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

impl<E> From<Error<E>> for ErrorInformation
where
    E: ErrorDiagnostic,
{
    fn from(err: Error<E>) -> Self {
        Self { inner: err.inner }
    }
}
