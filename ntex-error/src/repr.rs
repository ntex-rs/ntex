use std::{error, fmt, panic::Location, sync::Arc};

use crate::{Backtrace, Bytes, ErrorDiagnostic, ext::Extensions};

pub(crate) struct ErrorRepr<E> {
    pub(crate) error: E,
    pub(crate) tag: Option<Bytes>,
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
            tag: None,
            ext: Extensions::default(),
        }
    }
}

impl<E: Clone> ErrorRepr<E> {
    pub(crate) fn unpack(
        slf: Arc<ErrorRepr<E>>,
    ) -> (
        E,
        Option<Bytes>,
        Option<&'static str>,
        Backtrace,
        Extensions,
    ) {
        match Arc::try_unwrap(slf) {
            Ok(inner) => (
                inner.error,
                inner.tag,
                inner.service,
                inner.backtrace,
                inner.ext,
            ),
            Err(inner) => (
                inner.error.clone(),
                inner.tag.clone(),
                inner.service,
                inner.backtrace.clone(),
                inner.ext.clone(),
            ),
        }
    }

    pub(crate) fn with_mut<F>(mut slf: Arc<ErrorRepr<E>>, f: F) -> Arc<ErrorRepr<E>>
    where
        F: FnOnce(&mut ErrorRepr<E>),
    {
        if let Some(inner) = Arc::get_mut(&mut slf) {
            f(inner);
            slf
        } else {
            let mut inner = ErrorRepr::new2(
                slf.error.clone(),
                slf.tag.clone(),
                slf.service,
                slf.backtrace.clone(),
                slf.ext.clone(),
            );
            f(&mut inner);
            Arc::new(inner)
        }
    }
}

impl<E> ErrorRepr<E> {
    pub(crate) fn new2(
        error: E,
        tag: Option<Bytes>,
        service: Option<&'static str>,
        backtrace: Backtrace,
        ext: Extensions,
    ) -> Self {
        Self {
            error,
            tag,
            service,
            backtrace,
            ext,
        }
    }

    pub(crate) fn new3(
        error: E,
        tag: Option<Bytes>,
        service: Option<&'static str>,
        loc: &'static Location<'static>,
    ) -> Self {
        Self {
            error,
            tag,
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

    fn tag(&self) -> Option<&Bytes> {
        if self.tag.is_some() {
            self.tag.as_ref()
        } else {
            self.error.tag()
        }
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

impl<E> fmt::Debug for ErrorRepr<E>
where
    E: ErrorDiagnostic,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.error, f)
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
