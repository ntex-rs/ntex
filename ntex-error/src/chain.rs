use std::{error, fmt, panic::Location, sync::Arc};

use crate::{Backtrace, Error, ErrorDiagnostic, ErrorKind, ErrorRepr};

#[derive(Debug, Clone)]
pub struct ErrorChain<K: ErrorKind> {
    error: Arc<dyn ErrorDiagnostic<Kind = K>>,
}

impl<K: ErrorKind> ErrorChain<K> {
    #[track_caller]
    pub fn new<E>(error: E) -> Self
    where
        E: ErrorDiagnostic + Sized,
        K: ErrorKind + From<E::Kind>,
    {
        Self {
            error: Arc::new(ErrorRepr::new(error, None, Location::caller())),
        }
    }
}

impl<E, K> From<Error<E>> for ErrorChain<K>
where
    E: ErrorDiagnostic<Kind = K> + Sized,
    K: ErrorKind,
{
    fn from(err: Error<E>) -> Self {
        Self { error: err.inner }
    }
}

impl<K> error::Error for ErrorChain<K>
where
    K: ErrorKind,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.error.source()
    }
}

impl<K> fmt::Display for ErrorChain<K>
where
    K: ErrorKind,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<K> ErrorDiagnostic for ErrorChain<K>
where
    K: ErrorKind,
{
    type Kind = K;

    fn kind(&self) -> Self::Kind {
        self.error.kind()
    }

    fn service(&self) -> Option<&'static str> {
        self.error.service()
    }

    fn signature(&self) -> &'static str {
        self.error.signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.error.backtrace()
    }
}
