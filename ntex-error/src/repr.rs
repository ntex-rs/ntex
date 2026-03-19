use std::{error, fmt, panic::Location};

use crate::{Backtrace, ErrorDiagnostic, ext::Extensions};

pub(crate) struct ErrorRepr<E> {
    pub(crate) error: E,
    pub(crate) service: Option<&'static str>,
    pub(crate) backtrace: Backtrace,
    pub(crate) ext: Extensions,
}

impl<E: ErrorDiagnostic> ErrorRepr<E> {
    pub(crate) fn new(
        error: E,
        service: Option<&'static str>,
        loc: &'static Location<'static>,
    ) -> Self {
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
            ext: Extensions::default(),
        }
    }
}

impl<E> ErrorRepr<E> {
    pub(crate) fn new2(
        error: E,
        service: Option<&'static str>,
        backtrace: Backtrace,
        ext: Extensions,
    ) -> Self {
        Self {
            error,
            service,
            backtrace,
            ext,
        }
    }

    pub(crate) fn new3(
        error: E,
        service: Option<&'static str>,
        loc: &'static Location<'static>,
    ) -> Self {
        Self {
            error,
            service,
            ext: Extensions::default(),
            backtrace: Backtrace::new(loc),
        }
    }
}

impl<E> ErrorDiagnostic for ErrorRepr<E>
where
    E: ErrorDiagnostic,
{
    type Kind = E::Kind;

    fn kind(&self) -> Self::Kind {
        self.error.kind()
    }

    fn service(&self) -> Option<&'static str> {
        if self.service.is_some() {
            self.service
        } else {
            self.error.service()
        }
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        Some(&self.backtrace)
    }
}

impl<E> error::Error for ErrorRepr<E>
where
    E: ErrorDiagnostic,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.error)
    }
}

impl<E> fmt::Display for ErrorRepr<E>
where
    E: ErrorDiagnostic,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<E> fmt::Debug for ErrorRepr<E>
where
    E: ErrorDiagnostic,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.error, f)
    }
}
