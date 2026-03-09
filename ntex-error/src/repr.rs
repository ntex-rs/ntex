use std::{error, fmt, marker::PhantomData, panic::Location};

use crate::{Backtrace, ErrorDiagnostic, ErrorKind};

pub(crate) struct ErrorRepr<E, K> {
    pub(crate) error: E,
    pub(crate) service: Option<&'static str>,
    pub(crate) backtrace: Backtrace,
    _t: PhantomData<K>,
}

impl<E, K> ErrorRepr<E, K>
where
    E: ErrorDiagnostic,
    K: ErrorKind + From<E::Kind>,
{
    pub(crate) fn new(error: E, service: Option<&'static str>, loc: &Location<'_>) -> Self {
        let service = if service.is_none() {
            error.service()
        } else {
            service
        };
        let backtrace = if let Some(bt) = error.backtrace() {
            bt.clone()
        } else {
            Backtrace::new(loc)
        };

        Self {
            error,
            service,
            backtrace,
            _t: PhantomData,
        }
    }

    pub(crate) fn new2(
        error: E,
        service: Option<&'static str>,
        backtrace: Backtrace,
    ) -> Self {
        Self {
            error,
            service,
            backtrace,
            _t: PhantomData,
        }
    }
}

impl<E, K> ErrorDiagnostic for ErrorRepr<E, K>
where
    E: ErrorDiagnostic,
    K: ErrorKind + From<E::Kind>,
{
    type Kind = K;

    fn kind(&self) -> Self::Kind {
        self.error.kind().into()
    }

    fn service(&self) -> Option<&'static str> {
        if self.service.is_some() {
            self.service
        } else {
            self.error.service()
        }
    }

    fn signature(&self) -> &'static str {
        self.error.signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        Some(&self.backtrace)
    }
}

impl<E, K> Clone for ErrorRepr<E, K>
where
    E: Clone,
{
    fn clone(&self) -> Self {
        Self {
            error: self.error.clone(),
            service: self.service,
            backtrace: self.backtrace.clone(),
            _t: PhantomData,
        }
    }
}

impl<E, K> error::Error for ErrorRepr<E, K>
where
    E: ErrorDiagnostic,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.error)
    }
}

impl<E, K> fmt::Display for ErrorRepr<E, K>
where
    E: ErrorDiagnostic,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<E, K> fmt::Debug for ErrorRepr<E, K>
where
    E: ErrorDiagnostic,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("error", &self.error)
            .field("service", &self.service)
            .field("backtrace", &self.backtrace)
            .finish()
    }
}
