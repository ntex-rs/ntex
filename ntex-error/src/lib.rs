//! Error management.
use std::cell::RefCell;
use std::collections::HashMap;
use std::{error, fmt, fmt::Write, marker::PhantomData, ops, panic::Location, sync::Arc};

use backtrace::{Backtrace as StdBacktrace, BacktraceFrame};

thread_local! {
    static BACKTRACES: RefCell<HashMap<&'static Location<'static>, Backtrace>> = RefCell::new(HashMap::default());
}
static mut START: Option<(&'static str, u32)> = None;

#[track_caller]
pub fn set_backtrace_start(file: &'static str, line: u32) {
    unsafe {
        START = Some((file, line));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorType {
    Success,
    ClientError,
    ServiceError,
}

impl ErrorType {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ErrorType::Success => "Success",
            ErrorType::ClientError => "ClientError",
            ErrorType::ServiceError => "ServiceError",
        }
    }
}

pub trait ErrorKind: fmt::Display + fmt::Debug + 'static {
    /// Defines type of the error
    fn error_type(&self) -> ErrorType;
}

impl ErrorKind for ErrorType {
    fn error_type(&self) -> ErrorType {
        *self
    }
}

pub trait ErrorDiagnostic: error::Error + 'static {
    type Kind: ErrorKind;

    /// Provides specific kind of the error
    fn kind(&self) -> Self::Kind;

    /// Provides a string to identify responsible service
    fn service(&self) -> Option<&'static str> {
        None
    }

    /// Provides a string to identify specific kind of the error
    fn signature(&self) -> &'static str {
        self.kind().error_type().as_str()
    }

    /// Provides error call location
    fn backtrace(&self) -> Option<&Backtrace> {
        None
    }

    #[track_caller]
    fn chain(self) -> ErrorChain<Self::Kind>
    where
        Self: Sized,
    {
        ErrorChain::new(self)
    }
}

#[derive(Debug, Clone)]
pub struct Error<E> {
    error: E,
    service: Option<&'static str>,
    backtrace: Backtrace,
}

impl<E> Error<E> {
    pub const fn new(
        error: E,
        service: Option<&'static str>,
        backtrace: Backtrace,
    ) -> Self {
        Self {
            error,
            service,
            backtrace,
        }
    }

    /// Set response service
    pub fn set_service(mut self, name: &'static str) -> Self {
        self.service = Some(name);
        self
    }

    /// Map inner error to new error
    ///
    /// Keep same `service` and `location`
    pub fn map<U, F>(self, f: F) -> Error<U>
    where
        F: FnOnce(E) -> U,
    {
        Error {
            error: f(self.error),
            service: self.service,
            backtrace: self.backtrace,
        }
    }

    /// Get inner error value
    pub fn into_error(self) -> E {
        self.error
    }
}

impl<E: ErrorDiagnostic> From<E> for Error<E> {
    #[track_caller]
    fn from(error: E) -> Self {
        let bt = if let Some(bt) = error.backtrace() {
            bt.clone()
        } else {
            let loc = Location::caller();
            BACKTRACES.with(move |backtraces| {
                backtraces
                    .borrow_mut()
                    .entry(loc)
                    .or_insert_with(|| Backtrace::new(loc))
                    .clone()
            })
        };
        Self::new(error, None, bt)
    }
}

impl<E> Eq for Error<E> where E: Eq {}

impl<E> PartialEq for Error<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.error.eq(&other.error)
    }
}

impl<E> PartialEq<E> for Error<E>
where
    E: PartialEq,
{
    fn eq(&self, other: &E) -> bool {
        self.error.eq(other)
    }
}

impl<E> ops::Deref for Error<E> {
    type Target = E;

    fn deref(&self) -> &E {
        &self.error
    }
}

impl<E> error::Error for Error<E>
where
    E: ErrorDiagnostic,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.error)
    }
}

impl<E> ErrorDiagnostic for Error<E>
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

    fn signature(&self) -> &'static str {
        self.error.signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        Some(&self.backtrace)
    }
}

#[derive(Debug, Clone)]
pub struct ErrorChain<K: ErrorKind> {
    error: Arc<dyn ErrorDiagnostic<Kind = K>>,
}

impl<K: ErrorKind> ErrorChain<K> {
    #[track_caller]
    pub fn new<E>(error: E) -> Self
    where
        E: ErrorDiagnostic + Sized,
        E::Kind: Into<K>,
    {
        let service = error.service();
        let backtrace = if let Some(bt) = error.backtrace() {
            bt.clone()
        } else {
            let loc = Location::caller();
            BACKTRACES.with(move |backtraces| {
                backtraces
                    .borrow_mut()
                    .entry(loc)
                    .or_insert_with(|| Backtrace::new(loc))
                    .clone()
            })
        };

        Self {
            error: Arc::new(ErrorChainWrapper {
                error,
                service,
                backtrace,
                _k: PhantomData,
            }),
        }
    }
}

impl<E, K> From<Error<E>> for ErrorChain<K>
where
    E: ErrorDiagnostic + Sized,
    E::Kind: Into<K>,
    K: ErrorKind,
{
    fn from(err: Error<E>) -> Self {
        Self {
            error: Arc::new(ErrorChainWrapper {
                service: err.service(),
                error: err.error,
                backtrace: err.backtrace,
                _k: PhantomData,
            }),
        }
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

struct ErrorChainWrapper<E: Sized, K> {
    error: E,
    service: Option<&'static str>,
    backtrace: Backtrace,
    _k: PhantomData<K>,
}

impl<E, K> error::Error for ErrorChainWrapper<E, K>
where
    E: ErrorDiagnostic,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.error)
    }
}

impl<E, K> ErrorDiagnostic for ErrorChainWrapper<E, K>
where
    E: ErrorDiagnostic,
    E::Kind: Into<K>,
    K: ErrorKind,
{
    type Kind = K;

    fn kind(&self) -> Self::Kind {
        self.error.kind().into()
    }

    fn service(&self) -> Option<&'static str> {
        self.service
    }

    fn signature(&self) -> &'static str {
        self.error.signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        Some(&self.backtrace)
    }
}

#[derive(Clone)]
/// Representation of a backtrace.
///
/// This structure can be used to capture a backtrace at various
/// points in a program and later used to inspect what the backtrace
/// was at that time.
pub struct Backtrace(Arc<BacktraceInner>);

struct BacktraceInner {
    bt: StdBacktrace,
    repr: String,
}

impl Backtrace {
    fn new(loc: &Location<'_>) -> Self {
        let bt = StdBacktrace::new();

        let mut frames: Vec<BacktraceFrame> = bt.into();
        if let Some(idx) = find_loc(loc, &frames) {
            frames = frames.split_off(idx);
        }

        #[allow(static_mut_refs)]
        if let Some(start) = unsafe { START }
            && let Some(idx) = find_loc_start(start, &frames)
        {
            frames.truncate(idx);
        }

        let bt = frames.into();
        let mut repr = String::new();
        let _ = write!(&mut repr, "\n{:?}", bt);
        Self(Arc::new(BacktraceInner { bt, repr }))
    }

    /// Backtrace repr
    pub fn repr(&self) -> &str {
        &self.0.repr
    }

    /// Returns the frames from when this backtrace was captured.
    pub fn frames(&self) -> &[BacktraceFrame] {
        self.0.bt.frames()
    }
}

fn find_loc(loc: &Location<'_>, frames: &[BacktraceFrame]) -> Option<usize> {
    for (idx, frm) in frames.iter().enumerate() {
        for sym in frm.symbols() {
            if let Some(fname) = sym.filename()
                && let Some(lineno) = sym.lineno()
                && let Some(column) = sym.colno()
                && fname.ends_with(loc.file())
                && lineno == loc.line()
                && column == loc.column()
            {
                return Some(idx);
            }
        }
    }
    None
}

fn find_loc_start(loc: (&str, u32), frames: &[BacktraceFrame]) -> Option<usize> {
    let mut idx = frames.len();
    while idx > 0 {
        idx -= 1;
        let frm = &frames[idx];
        for sym in frm.symbols() {
            if let Some(fname) = sym.filename()
                && let Some(lineno) = sym.lineno()
                && fname.ends_with(loc.0)
                && lineno == loc.1
            {
                return Some(idx + 1);
            }
        }
    }
    None
}

impl fmt::Debug for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.repr, f)
    }
}

impl fmt::Display for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0.repr, f)
    }
}

impl<E> fmt::Display for Error<E>
where
    E: ErrorDiagnostic,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
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

impl<E, K> fmt::Display for ErrorChainWrapper<E, K>
where
    E: error::Error,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<E, K> fmt::Debug for ErrorChainWrapper<E, K>
where
    E: error::Error,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.error, f)
    }
}

impl fmt::Display for ErrorType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorType::Success => write!(f, "Success"),
            ErrorType::ClientError => write!(f, "ClientError"),
            ErrorType::ServiceError => write!(f, "ServiceError"),
        }
    }
}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, thiserror::Error)]
    enum TestKind {
        #[error("Connect")]
        Connect,
        #[error("Disconnect")]
        Disconnect,
        #[error("ServiceError")]
        ServiceError,
    }

    impl ErrorKind for TestKind {
        fn error_type(&self) -> ErrorType {
            match self {
                TestKind::Connect => ErrorType::ClientError,
                TestKind::Disconnect => ErrorType::ClientError,
                TestKind::ServiceError => ErrorType::ServiceError,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    enum TestError {
        #[error("Connect err: {0}")]
        Connect(&'static str),
        #[error("Disconnect")]
        Disconnect,
        #[error("InternalServiceError")]
        Service(&'static str),
    }

    impl ErrorDiagnostic for TestError {
        type Kind = TestKind;

        fn kind(&self) -> Self::Kind {
            match self {
                TestError::Connect(_) => TestKind::Connect,
                TestError::Disconnect => TestKind::Disconnect,
                TestError::Service(_) => TestKind::ServiceError,
            }
        }

        fn service(&self) -> Option<&'static str> {
            Some("test")
        }

        fn signature(&self) -> &'static str {
            match self {
                TestError::Connect(_) => "Client-Connect",
                TestError::Disconnect => "Client-Disconnect",
                TestError::Service(_) => "Service-Internal",
            }
        }
    }

    #[test]
    fn test_error() {
        let err: Error<TestError> = TestError::Service("409 Error").into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!((*err).kind(), TestKind::ServiceError);
        assert_eq!(err.to_string(), "InternalServiceError");
        assert_eq!(err.service(), Some("test"));
        assert_eq!(
            err,
            Into::<Error<TestError>>::into(TestError::Service("409 Error"))
        );
        assert!(err.backtrace().is_some());

        let err = err.set_service("SVC");
        assert_eq!(err.service(), Some("SVC"));

        assert_eq!(
            TestError::Connect("").kind().error_type(),
            ErrorType::ClientError
        );
        assert_eq!(
            TestError::Disconnect.kind().error_type(),
            ErrorType::ClientError
        );
        assert_eq!(
            TestError::Service("").kind().error_type(),
            ErrorType::ServiceError
        );
        assert_eq!(TestError::Connect("").to_string(), "Connect err: ");
        assert_eq!(TestError::Disconnect.to_string(), "Disconnect");
        assert_eq!(TestError::Disconnect.service(), Some("test"));
        assert!(TestError::Disconnect.backtrace().is_none());

        assert_eq!(ErrorType::Success.as_str(), "Success");
        assert_eq!(ErrorType::ClientError.as_str(), "ClientError");
        assert_eq!(ErrorType::ServiceError.as_str(), "ServiceError");
        assert_eq!(ErrorType::Success.error_type(), ErrorType::Success);
        assert_eq!(ErrorType::ClientError.error_type(), ErrorType::ClientError);
        assert_eq!(
            ErrorType::ServiceError.error_type(),
            ErrorType::ServiceError
        );
        assert_eq!(ErrorType::Success.to_string(), "Success");
        assert_eq!(ErrorType::ClientError.to_string(), "ClientError");
        assert_eq!(ErrorType::ServiceError.to_string(), "ServiceError");

        assert_eq!(TestKind::Connect.to_string(), "Connect");
        assert_eq!(TestError::Connect("").signature(), "Client-Connect");
        assert_eq!(TestKind::Disconnect.to_string(), "Disconnect");
        assert_eq!(TestError::Disconnect.signature(), "Client-Disconnect");
        assert_eq!(TestKind::ServiceError.to_string(), "ServiceError");
        assert_eq!(TestError::Service("").signature(), "Service-Internal");

        let err = err.into_error().chain();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!(err.kind(), TestError::Service("409 Error").kind());
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(format!("{:?}", err).contains("Service(\"409 Error\")"));

        let err: Error<TestError> = TestError::Service("404 Error").into();
        let err: ErrorChain<TestKind> = err.into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!(err.kind(), TestError::Service("404 Error").kind());
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.signature(), "Service-Internal");
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(err.backtrace().is_some());
        assert!(format!("{:?}", err).contains("Service(\"404 Error\")"));
    }
}
