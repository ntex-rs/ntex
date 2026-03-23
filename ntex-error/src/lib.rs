//! Error management.
#![deny(clippy::pedantic)]
#![allow(
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
use std::{error::Error as StdError, fmt};

mod bt;
mod chain;
mod error;
mod ext;
mod info;
mod message;
mod repr;
pub mod utils;

pub use crate::bt::{Backtrace, BacktraceResolver};
pub use crate::chain::ErrorChain;
pub use crate::error::Error;
pub use crate::info::ErrorInfo;
pub use crate::message::{ErrorMessage, ErrorMessageChained};
pub use crate::message::{fmt_diag, fmt_diag_string, fmt_err, fmt_err_string};
pub use crate::utils::{Success, with_service};

#[doc(hidden)]
pub use crate::bt::{set_backtrace_start, set_backtrace_start_alt};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultType {
    Success,
    ClientError,
    ServiceError,
    #[doc(hidden)]
    #[deprecated]
    Client,
    #[doc(hidden)]
    #[deprecated]
    Service,
}

#[deprecated]
#[doc(hidden)]
pub type ErrorType = ResultType;

impl ResultType {
    #[allow(deprecated)]
    pub const fn as_str(&self) -> &'static str {
        match self {
            ResultType::Success => "Success",
            ResultType::ClientError | ResultType::Client => "ClientError",
            ResultType::ServiceError | ResultType::Service => "ServiceError",
        }
    }
}

pub trait ResultKind: fmt::Debug + 'static {
    /// Defines type of the error
    fn tp(&self) -> ResultType;

    /// Error signature
    fn signature(&self) -> &'static str;
}

pub trait ErrorKind: fmt::Debug + 'static {
    /// Defines type of the error
    fn tp(&self) -> ResultType;

    /// Error signature
    fn signature(&self) -> &'static str;
}

impl ResultKind for ResultType {
    fn tp(&self) -> ResultType {
        *self
    }

    fn signature(&self) -> &'static str {
        self.as_str()
    }
}

pub trait ErrorDiagnostic: StdError + 'static {
    type Kind: ResultKind;

    /// Provides specific kind of the error
    fn kind(&self) -> Self::Kind;

    /// Provides a string to identify responsible service
    fn service(&self) -> Option<&'static str> {
        None
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

pub trait ErrorMapping<T, E, U> {
    fn into_error(self) -> Result<T, Error<U>>;
}

impl fmt::Display for ResultType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[allow(dead_code)]
#[cfg(test)]
mod tests {
    use std::{error::Error as StdError, io, mem};

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

    impl ResultKind for TestKind {
        fn tp(&self) -> ResultType {
            match self {
                TestKind::Connect | TestKind::Disconnect => ResultType::ClientError,
                TestKind::ServiceError => ResultType::ServiceError,
            }
        }

        fn signature(&self) -> &'static str {
            match self {
                TestKind::Connect => "Client-Connect",
                TestKind::Disconnect => "Client-Disconnect",
                TestKind::ServiceError => "Service-Internal",
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
    }

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    #[error("TestError2")]
    struct TestError2;
    impl ErrorDiagnostic for TestError2 {
        type Kind = ResultType;

        fn kind(&self) -> Self::Kind {
            ResultType::ClientError
        }
    }

    impl From<TestError> for TestError2 {
        fn from(_err: TestError) -> TestError2 {
            TestError2
        }
    }

    #[ntex::test]
    async fn test_error() {
        let err: Error<TestError> = TestError::Service("409 Error").into();
        let err = err.clone();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!((*err).kind(), TestKind::ServiceError);
        assert_eq!(err.to_string(), "InternalServiceError");
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.kind().signature(), "Service-Internal");
        assert_eq!(
            err,
            Into::<Error<TestError>>::into(TestError::Service("409 Error"))
        );
        assert!(err.backtrace().is_some());

        let err = err.set_service("SVC");
        assert_eq!(err.service(), Some("SVC"));

        let err2: Error<TestError> = Error::new(TestError::Service("409 Error"), "TEST");
        assert!(err != err2);
        assert_eq!(err, TestError::Service("409 Error"));

        let err2 = err2.set_service("SVC");
        assert_eq!(err, err2);
        let err2 = err2.map(|_| TestError::Disconnect);
        assert!(err != err2);
        let err2 = err2.forward(|_| TestError::Disconnect);
        assert!(err != err2);

        assert_eq!(TestError::Connect("").kind().tp(), ResultType::ClientError);
        assert_eq!(TestError::Disconnect.kind().tp(), ResultType::ClientError);
        assert_eq!(TestError::Service("").kind().tp(), ResultType::ServiceError);
        assert_eq!(TestError::Connect("").to_string(), "Connect err: ");
        assert_eq!(TestError::Disconnect.to_string(), "Disconnect");
        assert_eq!(TestError::Disconnect.service(), Some("test"));
        assert!(TestError::Disconnect.backtrace().is_none());

        assert_eq!(ResultType::ClientError.as_str(), "ClientError");
        assert_eq!(ResultType::ServiceError.as_str(), "ServiceError");
        assert_eq!(ResultType::ClientError.tp(), ResultType::ClientError);
        assert_eq!(ResultType::ServiceError.tp(), ResultType::ServiceError);
        assert_eq!(ResultType::ClientError.to_string(), "ClientError");
        assert_eq!(ResultType::ServiceError.to_string(), "ServiceError");
        assert_eq!(format!("{}", ResultType::ClientError), "ClientError");

        assert_eq!(TestKind::Connect.to_string(), "Connect");
        assert_eq!(TestError::Connect("").kind().signature(), "Client-Connect");
        assert_eq!(TestKind::Disconnect.to_string(), "Disconnect");
        assert_eq!(
            TestError::Disconnect.kind().signature(),
            "Client-Disconnect"
        );
        assert_eq!(TestKind::ServiceError.to_string(), "ServiceError");
        assert_eq!(
            TestError::Service("").kind().signature(),
            "Service-Internal"
        );

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
        if let Some(bt) = err.backtrace() {
            bt.resolver().resolve();
            assert!(
                format!("{bt}").contains("ntex_error::tests::test_error"),
                "{bt}",
            );
            assert!(
                bt.repr().unwrap().contains("ntex_error::tests::test_error"),
                "{bt}"
            );
        }

        let err: ErrorChain<TestKind> = err.into();
        assert_eq!(err.kind(), TestKind::ServiceError);
        assert_eq!(err.kind(), TestError::Service("404 Error").kind());
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.kind().signature(), "Service-Internal");
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(err.backtrace().is_some());
        assert!(format!("{err:?}").contains("Service(\"404 Error\")"));

        assert_eq!(24, mem::size_of::<TestError>());
        assert_eq!(8, mem::size_of::<Error<TestError>>());

        assert_eq!(TestError2.service(), None);
        assert_eq!(TestError2.kind().signature(), "ClientError");

        // ErrorInformation
        let err: Error<TestError> = TestError::Service("409 Error").into();
        let msg = fmt_err_string(&err);
        assert_eq!(msg, "InternalServiceError\n");
        let msg = fmt_diag_string(&err);
        assert!(msg.contains("err: InternalServiceError"));

        let err: ErrorInfo = err.set_service("SVC").into();
        assert_eq!(err.tp(), ResultType::ServiceError);
        assert_eq!(err.service(), Some("SVC"));
        assert_eq!(err.signature(), "Service-Internal");
        assert!(err.backtrace().is_some());

        let res = Err(TestError::Service("409 Error"));
        let res: Result<(), Error<TestError>> = res.into_error();
        let _res: Result<(), Error<TestError2>> = res.into_error();

        let msg = fmt_err_string(&err);
        assert_eq!(msg, "InternalServiceError\n");
        let msg = fmt_diag_string(&err);
        assert!(msg.contains("err: InternalServiceError"));

        // Error extensions
        let err: Error<TestError> = TestError::Service("409 Error").into();
        assert_eq!(err.get_item::<&str>(), None);
        let err = err.insert_item("Test");
        assert_eq!(err.get_item::<&str>(), Some(&"Test"));
        let err2 = err.clone();
        assert_eq!(err2.get_item::<&str>(), Some(&"Test"));
        let err2 = err2.insert_item("Test2");
        assert_eq!(err2.get_item::<&str>(), Some(&"Test2"));
        assert_eq!(err.get_item::<&str>(), Some(&"Test"));
        let err2 = err.clone().map(|_| TestError::Disconnect);
        assert_eq!(err2.get_item::<&str>(), Some(&"Test"));

        let info = ErrorInfo::from(&err2);
        assert_eq!(info.get_item::<&str>(), Some(&"Test"));

        let err3 = err
            .clone()
            .try_map(|_| Err::<(), _>(TestError2))
            .err()
            .unwrap();
        assert_eq!(err3.kind().signature(), "ClientError");
        assert_eq!(err3.get_item::<&str>(), Some(&"Test"));

        let res = err.clone().try_map(|_| Ok::<_, TestError2>(()));
        assert_eq!(res, Ok(()));

        assert_eq!(Success.kind(), ResultType::Success);
        assert_eq!(format!("{Success}"), "Success");

        assert_eq!(
            ErrorDiagnostic::kind(&io::Error::other("")),
            ResultType::ServiceError
        );
        assert_eq!(
            ErrorDiagnostic::kind(&io::Error::new(io::ErrorKind::InvalidData, "")),
            ResultType::ClientError
        );
    }
}
