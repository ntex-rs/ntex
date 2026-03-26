use std::{error, fmt, ops, panic::Location, sync::Arc};

use crate::{Backtrace, Bytes, ErrorDiagnostic, ErrorMapping, repr::ErrorRepr};

/// An error container.
///
/// `Error<E>` is a lightweight handle to an error that can be cheaply cloned
/// and safely shared across threads. It preserves the original error along with
/// associated context such as where it occurred.
pub struct Error<E> {
    pub(crate) inner: Arc<ErrorRepr<E>>,
}

impl<E> Error<E> {
    /// Creates a new error container.
    ///
    /// Captures the caller location and associates the error with a service.
    #[track_caller]
    pub fn new<T>(error: T, service: &'static str) -> Self
    where
        E: From<T>,
    {
        Self {
            inner: Arc::new(ErrorRepr::new3(
                E::from(error),
                None,
                Some(service),
                Location::caller(),
            )),
        }
    }

    /// Transforms the inner error into another error type.
    ///
    /// Preserves `service`, backtrace, and extension data.
    pub fn forward<U, F>(self, f: F) -> Error<U>
    where
        F: FnOnce(Error<E>) -> U,
    {
        let svc = self.inner.service;
        let tag = self.inner.tag.clone();
        let bt = self.inner.backtrace.clone();
        let ext = self.inner.ext.clone();

        Error {
            inner: Arc::new(ErrorRepr::new2(f(self), tag, svc, bt, ext)),
        }
    }

    /// Returns a debug view of the error.
    ///
    /// Intended for debugging purposes.
    pub fn debug(&self) -> impl fmt::Debug
    where
        E: fmt::Debug,
    {
        ErrorDebug {
            inner: self.inner.as_ref(),
        }
    }

    /// Returns a reference to a previously stored value of type `T` from this error.
    ///
    /// This can be used to access additional contextual data attached to the error.
    pub fn get_item<T: 'static>(&self) -> Option<&T> {
        self.inner.ext.get::<T>()
    }
}

impl<E: Clone> Error<E> {
    /// Sets a user-defined tag on this error.
    ///
    /// Returns the updated error.
    #[must_use]
    pub fn set_tag<T: Into<Bytes>>(self, tag: T) -> Self {
        Error {
            inner: ErrorRepr::with_mut(self.inner, move |inner| {
                inner.tag = Some(tag.into());
            }),
        }
    }

    /// Sets the service responsible for this error.
    ///
    /// Returns the updated error.
    #[must_use]
    pub fn set_service(self, name: &'static str) -> Self {
        Error {
            inner: ErrorRepr::with_mut(self.inner, move |inner| {
                inner.service = Some(name);
            }),
        }
    }

    /// Maps the inner error into a new error type.
    ///
    /// Preserves `service`, backtrace, and extension data.
    pub fn map<U, F>(self, f: F) -> Error<U>
    where
        F: FnOnce(E) -> U,
    {
        let (err, tag, svc, bt, ext) = ErrorRepr::unpack(self.inner);

        Error {
            inner: Arc::new(ErrorRepr::new2(f(err), tag, svc, bt, ext)),
        }
    }

    /// Try to map inner error to new error.
    ///
    /// Preserves `service`, backtrace, and extension data.
    pub fn try_map<T, U, F>(self, f: F) -> Result<T, Error<U>>
    where
        F: FnOnce(E) -> Result<T, U>,
    {
        let (err, tag, svc, bt, ext) = ErrorRepr::unpack(self.inner);

        f(err).map_err(move |err| Error {
            inner: Arc::new(ErrorRepr::new2(err, tag, svc, bt, ext)),
        })
    }

    /// Attaches a typed value to this `Error`.
    ///
    /// This value can be retrieved later using `get_item::<T>()`.
    #[must_use]
    pub fn insert_item<T: Sync + Send + 'static>(self, val: T) -> Self {
        Error {
            inner: ErrorRepr::with_mut(self.inner, move |inner| {
                inner.ext.insert(val);
            }),
        }
    }
}

impl<E: Clone> Error<E> {
    /// Consumes this error and returns the inner error value.
    pub fn into_error(self) -> E {
        Arc::try_unwrap(self.inner)
            .map_or_else(|inner| inner.error.clone(), |inner| inner.error)
    }
}

impl<E> Clone for Error<E> {
    fn clone(&self) -> Error<E> {
        Error {
            inner: self.inner.clone(),
        }
    }
}

impl<E> From<E> for Error<E> {
    #[track_caller]
    fn from(error: E) -> Self {
        Self {
            inner: Arc::new(ErrorRepr::new3(error, None, None, Location::caller())),
        }
    }
}

impl<E> Eq for Error<E> where E: Eq {}

impl<E> PartialEq for Error<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.inner.error.eq(&other.inner.error) && self.inner.service == other.inner.service
    }
}

impl<E> PartialEq<E> for Error<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &E) -> bool {
        self.inner.error.eq(other)
    }
}

impl<E> ops::Deref for Error<E> {
    type Target = E;

    fn deref(&self) -> &E {
        &self.inner.error
    }
}

impl<E: error::Error + 'static> error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.inner.error.source()
    }
}

impl<E: ErrorDiagnostic> ErrorDiagnostic for Error<E> {
    type Kind = E::Kind;

    fn kind(&self) -> Self::Kind {
        self.inner.kind()
    }

    fn tag(&self) -> Option<&Bytes> {
        self.inner.tag()
    }

    fn service(&self) -> Option<&'static str> {
        self.inner.service()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }
}

impl<T, E, U> ErrorMapping<T, E, U> for Result<T, E>
where
    U: From<E>,
{
    fn into_error(self) -> Result<T, Error<U>> {
        match self {
            Ok(val) => Ok(val),
            Err(err) => Err(Error {
                inner: Arc::new(ErrorRepr::new3(
                    U::from(err),
                    None,
                    None,
                    Location::caller(),
                )),
            }),
        }
    }
}

impl<T, E, U> ErrorMapping<T, E, U> for Result<T, Error<E>>
where
    U: From<E>,
    E: Clone,
{
    fn into_error(self) -> Result<T, Error<U>> {
        match self {
            Ok(val) => Ok(val),
            Err(err) => Err(err.map(U::from)),
        }
    }
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner.error, f)
    }
}

impl<E: fmt::Debug> fmt::Debug for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner.error, f)
    }
}

struct ErrorDebug<'a, E> {
    inner: &'a ErrorRepr<E>,
}

impl<E: fmt::Debug> fmt::Debug for ErrorDebug<'_, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("error", &self.inner.error)
            .field("service", &self.inner.service)
            .field("backtrace", &self.inner.backtrace)
            .finish()
    }
}
