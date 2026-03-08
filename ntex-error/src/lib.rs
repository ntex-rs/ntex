//! Error management.
#![deny(clippy::pedantic)]
#![allow(clippy::must_use_candidate)]
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::marker::PhantomData;
use std::panic::Location;
use std::{cell::RefCell, error, fmt, fmt::Write, ops, os, ptr, sync::Arc};

use backtrace::{BacktraceFmt, BacktraceFrame, BytesOrWideString};

thread_local! {
    static FRAMES: RefCell<HashMap<*mut os::raw::c_void, BacktraceFrame>> = RefCell::new(HashMap::default());
    static REPRS: RefCell<HashMap<u64, Arc<str>>> = RefCell::new(HashMap::default());
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
    Client,
    Service,
}

impl ErrorType {
    pub const fn as_str(&self) -> &'static str {
        match self {
            ErrorType::Client => "ClientError",
            ErrorType::Service => "ServiceError",
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

    /// Check if error is service related
    fn is_service(&self) -> bool {
        self.kind().error_type() == ErrorType::Service
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
    inner: Box<ErrorInner<E>>,
}

#[derive(Debug, Clone)]
struct ErrorInner<E> {
    error: E,
    service: Option<&'static str>,
    backtrace: Backtrace,
}

impl<E> Error<E> {
    #[track_caller]
    pub fn new<T>(error: T, service: &'static str) -> Self
    where
        E: ErrorDiagnostic,
        E: From<T>,
    {
        let error = E::from(error);
        let backtrace = if let Some(bt) = error.backtrace() {
            bt.clone()
        } else {
            Backtrace::new(Location::caller())
        };
        Self {
            inner: Box::new(ErrorInner {
                error,
                backtrace,
                service: Some(service),
            }),
        }
    }

    #[must_use]
    /// Set response service
    pub fn set_service(mut self, name: &'static str) -> Self {
        self.inner.service = Some(name);
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
            inner: Box::new(ErrorInner {
                error: f(self.inner.error),
                service: self.inner.service,
                backtrace: self.inner.backtrace,
            }),
        }
    }

    /// Get inner error value
    pub fn into_error(self) -> E {
        self.inner.error
    }
}

impl<E: ErrorDiagnostic> From<E> for Error<E> {
    #[track_caller]
    fn from(error: E) -> Self {
        let backtrace = if let Some(bt) = error.backtrace() {
            bt.clone()
        } else {
            Backtrace::new(Location::caller())
        };
        Self {
            inner: Box::new(ErrorInner {
                error,
                backtrace,
                service: None,
            }),
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

impl<E> error::Error for Error<E>
where
    E: ErrorDiagnostic,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.inner.error)
    }
}

impl<E> ErrorDiagnostic for Error<E>
where
    E: ErrorDiagnostic,
{
    type Kind = E::Kind;

    fn kind(&self) -> Self::Kind {
        self.inner.error.kind()
    }

    fn service(&self) -> Option<&'static str> {
        if self.inner.service.is_some() {
            self.inner.service
        } else {
            self.inner.error.service()
        }
    }

    fn signature(&self) -> &'static str {
        self.inner.error.signature()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        Some(&self.inner.backtrace)
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
            Backtrace::new(Location::caller())
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
                error: err.inner.error,
                backtrace: err.inner.backtrace,
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
pub struct Backtrace(Arc<str>);

impl Backtrace {
    fn new(loc: &Location<'_>) -> Self {
        let repr = FRAMES.with(|c| {
            let mut cache = c.borrow_mut();
            let mut idx = 0;
            let mut st = foldhash::fast::FixedState::default().build_hasher();
            let mut idxs: [*mut os::raw::c_void; 128] = [ptr::null_mut(); 128];

            backtrace::trace(|frm| {
                let ip = frm.ip();
                st.write_usize(ip as usize);
                cache.entry(ip).or_insert_with(|| {
                    let mut f = BacktraceFrame::from(frm.clone());
                    f.resolve();
                    f
                });
                idxs[idx] = ip;
                idx += 1;

                idx < 128
            });

            let id = st.finish();

            REPRS.with(|r| {
                let mut reprs = r.borrow_mut();
                if let Some(repr) = reprs.get(&id) {
                    repr.clone()
                } else {
                    let mut frames: [Option<&BacktraceFrame>; 128] = [None; 128];
                    for (idx, ip) in idxs.as_ref().iter().enumerate() {
                        if !ip.is_null() {
                            frames[idx] = Some(&cache[ip]);
                        }
                    }

                    find_loc(loc, &mut frames);

                    #[allow(static_mut_refs)]
                    if let Some(start) = unsafe { START } {
                        find_loc_start(start, &mut frames);
                    }

                    let bt = Bt(&frames[..]);
                    let mut buf = String::new();
                    let _ = write!(&mut buf, "\n{bt:?}");
                    let repr: Arc<str> = Arc::from(buf);
                    reprs.insert(id, repr.clone());
                    repr
                }
            })
        });

        Self(repr)
    }

    /// Backtrace repr
    pub fn repr(&self) -> &str {
        &self.0
    }
}

fn find_loc(loc: &Location<'_>, frames: &mut [Option<&BacktraceFrame>]) {
    for (idx, frm) in frames.iter_mut().enumerate() {
        if let Some(f) = frm {
            for sym in f.symbols() {
                if let Some(fname) = sym.filename()
                    && let Some(lineno) = sym.lineno()
                    && fname.ends_with(loc.file())
                    && lineno == loc.line()
                {
                    for f in frames.iter_mut().take(idx) {
                        *f = None;
                    }
                    return;
                }
            }
        } else {
            break;
        }
    }
}

fn find_loc_start(loc: (&str, u32), frames: &mut [Option<&BacktraceFrame>]) {
    let mut idx = frames.len();
    while idx > 0 {
        idx -= 1;
        if let Some(frm) = &frames[idx] {
            for sym in frm.symbols() {
                if let Some(fname) = sym.filename()
                    && let Some(lineno) = sym.lineno()
                    && fname.ends_with(loc.0)
                    && lineno == loc.1
                {
                    for f in frames.iter_mut().skip(idx + 1) {
                        if f.is_some() {
                            *f = None;
                        } else {
                            return;
                        }
                    }
                }
            }
        }
    }
}

struct Bt<'a>(&'a [Option<&'a BacktraceFrame>]);

impl fmt::Debug for Bt<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cwd = std::env::current_dir();
        let mut print_path =
            move |fmt: &mut fmt::Formatter<'_>, path: BytesOrWideString<'_>| {
                let path = path.into_path_buf();
                if let Ok(cwd) = &cwd
                    && let Ok(suffix) = path.strip_prefix(cwd)
                {
                    return fmt::Display::fmt(&suffix.display(), fmt);
                }
                fmt::Display::fmt(&path.display(), fmt)
            };

        let mut f = BacktraceFmt::new(fmt, backtrace::PrintFmt::Short, &mut print_path);
        f.add_context()?;
        for frm in self.0.iter().flatten() {
            f.frame().backtrace_frame(frm)?;
        }
        f.finish()?;
        Ok(())
    }
}

impl fmt::Debug for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Display for Backtrace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl<E> fmt::Display for Error<E>
where
    E: ErrorDiagnostic,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.inner.error, f)
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
            ErrorType::Client => write!(f, "ClientError"),
            ErrorType::Service => write!(f, "ServiceError"),
        }
    }
}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
    use std::{error::Error as StdError, mem};

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
                TestKind::Connect | TestKind::Disconnect => ErrorType::Client,
                TestKind::ServiceError => ErrorType::Service,
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

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    #[error("TestError2")]
    struct TestError2;
    impl ErrorDiagnostic for TestError2 {
        type Kind = ErrorType;

        fn kind(&self) -> Self::Kind {
            ErrorType::Client
        }
    }

    #[test]
    fn test_error() {
        let err: Error<TestError> = TestError::Service("409 Error").into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!((*err).kind(), TestKind::ServiceError);
        assert_eq!(err.to_string(), "InternalServiceError");
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.signature(), "Service-Internal");
        assert_eq!(
            err,
            Into::<Error<TestError>>::into(TestError::Service("409 Error"))
        );
        assert!(err.backtrace().is_some());
        assert!(err.is_service());
        assert!(
            format!("{:?}", err.source()).contains("Service(\"409 Error\")"),
            "{:?}",
            err.source().unwrap()
        );

        let err = err.set_service("SVC");
        assert_eq!(err.service(), Some("SVC"));

        let err2: Error<TestError> = Error::new(TestError::Service("409 Error"), "TEST");
        assert!(err != err2);
        assert_eq!(err, TestError::Service("409 Error"));

        let err2 = err2.set_service("SVC");
        assert_eq!(err, err2);
        let err2 = err2.map(|_| TestError::Disconnect);
        assert!(err != err2);

        assert_eq!(
            TestError::Connect("").kind().error_type(),
            ErrorType::Client
        );
        assert_eq!(TestError::Disconnect.kind().error_type(), ErrorType::Client);
        assert_eq!(
            TestError::Service("").kind().error_type(),
            ErrorType::Service
        );
        assert_eq!(TestError::Connect("").to_string(), "Connect err: ");
        assert_eq!(TestError::Disconnect.to_string(), "Disconnect");
        assert_eq!(TestError::Disconnect.service(), Some("test"));
        assert!(TestError::Disconnect.backtrace().is_none());

        assert_eq!(ErrorType::Client.as_str(), "ClientError");
        assert_eq!(ErrorType::Service.as_str(), "ServiceError");
        assert_eq!(ErrorType::Client.error_type(), ErrorType::Client);
        assert_eq!(ErrorType::Service.error_type(), ErrorType::Service);
        assert_eq!(ErrorType::Client.to_string(), "ClientError");
        assert_eq!(ErrorType::Service.to_string(), "ServiceError");

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
        assert!(format!("{err:?}").contains("Service(\"409 Error\")"));
        assert!(
            format!("{:?}", err.source()).contains("Service(\"409 Error\")"),
            "{:?}",
            err.source().unwrap()
        );

        let err: Error<TestError> = TestError::Service("404 Error").into();
        let err: ErrorChain<TestKind> = err.into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!(err.kind(), TestError::Service("404 Error").kind());
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.signature(), "Service-Internal");
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(err.backtrace().is_some());
        assert!(format!("{err:?}").contains("Service(\"404 Error\")"));

        assert_eq!(24, mem::size_of::<TestError>());
        assert_eq!(8, mem::size_of::<Error<TestError>>());

        assert_eq!(TestError2.service(), None);
        assert_eq!(TestError2.signature(), "ClientError");
    }
}
