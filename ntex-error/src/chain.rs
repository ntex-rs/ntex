use std::{error, fmt, panic::Location, sync::Arc};

use crate::{Backtrace, Bytes, Error, ErrorDiagnostic, ResultKind, repr::ErrorRepr};

/// A type-erased container representing an error within a diagnostic chain.
///
/// This allows errors to be stored and propagated without exposing concrete types.
#[derive(Clone)]
pub struct ErrorChain<K: ResultKind> {
    error: Arc<dyn ErrorDiagnostic<Kind = K>>,
}

impl<K: ResultKind> ErrorChain<K> {
    #[track_caller]
    pub fn new<E>(error: E) -> Self
    where
        E: ErrorDiagnostic<Kind = K> + Sized,
    {
        Self {
            error: Arc::new(ErrorRepr::new(error, None, Location::caller())),
        }
    }
}

impl<E, K> From<Error<E>> for ErrorChain<K>
where
    E: ErrorDiagnostic<Kind = K> + Sized,
    K: ResultKind,
{
    fn from(err: Error<E>) -> Self {
        Self { error: err.inner }
    }
}

impl<K> error::Error for ErrorChain<K>
where
    K: ResultKind,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.error.source()
    }
}

impl<K> fmt::Debug for ErrorChain<K>
where
    K: ResultKind,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.error, f)
    }
}

impl<K> fmt::Display for ErrorChain<K>
where
    K: ResultKind,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<K> ErrorDiagnostic for ErrorChain<K>
where
    K: ResultKind,
{
    type Kind = K;

    fn kind(&self) -> Self::Kind {
        self.error.kind()
    }

    fn tag(&self) -> Option<&Bytes> {
        self.error.tag()
    }

    fn service(&self) -> Option<&'static str> {
        self.error.service()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.error.backtrace()
    }
}
