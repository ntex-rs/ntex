//! Error management.
#![deny(clippy::pedantic)]
#![allow(
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
use std::{error::Error as StdError, fmt};

use ntex_bytes::Bytes;

mod bt;
mod error;
mod ext;
mod info;
mod message;
mod repr;
pub mod utils;

pub use crate::bt::{Backtrace, BacktraceResolver};
pub use crate::error::Error;
pub use crate::info::ErrorInfo;
pub use crate::message::{ErrorMessage, ErrorMessageChained};
pub use crate::message::{
    fmt_diag, fmt_diag_string, fmt_diag_typ, fmt_err, fmt_err_string,
};
pub use crate::utils::{ResultSignature, Retryable, Success, with_service};

#[doc(hidden)]
pub use crate::bt::{set_backtrace_start, set_backtrace_start_alt};

/// The type of the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ResultType {
    Success,
    ClientError,
    ServiceError,
}

impl ResultType {
    /// Returns a str representation of the result type.
    pub const fn as_str(&self) -> &'static str {
        match self {
            ResultType::Success => "Success",
            ResultType::ClientError => "ClientError",
            ResultType::ServiceError => "ServiceError",
        }
    }
}

pub trait AsError {
    type Target: ErrorDiagnostic;

    fn as_diag(&self) -> &Self::Target;
}

/// Provides diagnostic information for errors.
///
/// It enables classification, service attribution, and debugging context.
pub trait ErrorDiagnostic: StdError + 'static {
    #[doc(hidden)]
    #[deprecated(since = "2.1.0")]
    /// Returns the classification of the result (e.g. success, client error, service error).
    fn typ(&self) -> ResultType {
        ResultType::ServiceError
    }

    /// Returns a stable identifier for the specific error classification.
    ///
    /// It is used for logging, metrics, and diagnostics.
    fn signature(&self) -> &'static str;

    /// Returns an optional tag associated with this error.
    ///
    /// The tag is user-defined and can be used for additional classification
    /// or correlation.
    fn tag(&self) -> Option<&Bytes> {
        None
    }

    /// Returns the name of the responsible service, if applicable.
    ///
    /// Used to identify upstream or internal service ownership for diagnostics.
    fn service(&self) -> Option<&'static str> {
        None
    }

    /// Returns a backtrace for debugging purposes, if available.
    fn backtrace(&self) -> Option<&Backtrace> {
        None
    }
}

/// Helper trait for converting a value into a unified error-aware result type.
pub trait ErrorMapping<T, E, U> {
    /// Converts the value into a `Result`, wrapping it in a structured error type if needed.
    fn into_error(self) -> Result<T, Error<U>>;
}

impl fmt::Display for ResultType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl ErrorDiagnostic for ResultType {
    fn typ(&self) -> ResultType {
        *self
    }

    fn signature(&self) -> &'static str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error as StdError, mem};

    use super::*;

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
        fn signature(&self) -> &'static str {
            match self {
                TestError::Connect(_) => "Client-Connect",
                TestError::Disconnect => "Client-Disconnect",
                TestError::Service(_) => "Service-Internal",
            }
        }

        fn service(&self) -> Option<&'static str> {
            Some("test")
        }
    }

    impl From<&TestError> for ResultType {
        fn from(err: &TestError) -> ResultType {
            match err {
                TestError::Connect(_) | TestError::Disconnect => ResultType::ClientError,
                TestError::Service(_) => ResultType::ServiceError,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
    #[error("TestError2")]
    struct TestError2;
    impl ErrorDiagnostic for TestError2 {
        fn typ(&self) -> ResultType {
            ResultType::ClientError
        }

        fn signature(&self) -> &'static str {
            "TestError2"
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
        assert_eq!(err.to_string(), "InternalServiceError");
        assert_eq!(err.service(), Some("test"));
        assert_eq!(err.signature(), "Service-Internal");
        assert_eq!(
            err,
            Into::<Error<TestError>>::into(TestError::Service("409 Error"))
        );
        assert!(err.backtrace().is_some());

        let err = err.set_service("SVC");
        assert_eq!(err.service(), Some("SVC"));
        let err = err.set_tag("TAG");
        assert_eq!(err.tag().unwrap(), &b"TAG"[..]);

        let err2: Error<TestError> = Error::new(TestError::Service("409 Error"), "TEST");
        assert!(err != err2);
        assert_eq!(err, TestError::Service("409 Error"));

        let err2 = err2.set_tag("TAG");
        assert_eq!(err.tag().unwrap(), &b"TAG"[..]);
        let err2 = err2.set_service("SVC");
        assert_eq!(err, err2);
        let err2 = err2.map(|_| TestError::Disconnect);
        assert!(err != err2);
        let err2 = err2.forward(|_| TestError::Disconnect);
        assert!(err != err2);

        assert_eq!(TestError::Connect("").to_string(), "Connect err: ");
        assert_eq!(TestError::Disconnect.to_string(), "Disconnect");
        assert_eq!(TestError::Disconnect.service(), Some("test"));
        assert!(TestError::Disconnect.backtrace().is_none());

        assert_eq!(ResultType::ClientError.as_str(), "ClientError");
        assert_eq!(ResultType::ServiceError.as_str(), "ServiceError");
        assert_eq!(ResultType::ClientError.to_string(), "ClientError");
        assert_eq!(ResultType::ServiceError.to_string(), "ServiceError");
        assert_eq!(format!("{}", ResultType::ClientError), "ClientError");

        assert_eq!(TestError::Connect("").signature(), "Client-Connect");
        assert_eq!(TestError::Disconnect.signature(), "Client-Disconnect");
        assert_eq!(TestError::Service("").signature(), "Service-Internal");

        let err = err.into_error();
        assert_eq!(err.to_string(), "InternalServiceError");
        assert!(err.source().is_none());
        assert!(format!("{err:?}").contains("Service(\"409 Error\")"));

        #[cfg(unix)]
        {
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
        }

        assert_eq!(24, mem::size_of::<TestError>());
        assert_eq!(8, mem::size_of::<Error<TestError>>());

        assert_eq!(TestError2.service(), None);
        assert_eq!(TestError2.signature(), "TestError2");

        // ErrorInformation
        let err: Error<TestError> = TestError::Service("409 Error").into();
        let msg = fmt_err_string(&err);
        assert_eq!(msg, "InternalServiceError\n");
        let msg = fmt_diag_string(&err);
        assert!(msg.contains("err: InternalServiceError"));

        let err: ErrorInfo = err.set_service("SVC").into();
        assert_eq!(err.service(), Some("SVC"));
        assert_eq!(err.signature(), "Service-Internal");
        assert!(err.backtrace().is_some());

        let res = Err(TestError::Service("409 Error"));
        let res: Result<(), Error<TestError>> = res.into_error();
        let _res: Result<(), Error<TestError2>> = res.into_error();

        let msg = fmt_err_string(&err);
        assert_eq!(msg, "InternalServiceError\n");

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
        assert_eq!(err3.signature(), "TestError2");
        assert_eq!(err3.get_item::<&str>(), Some(&"Test"));

        let res = err.clone().try_map(|_| Ok::<_, TestError2>(()));
        assert_eq!(res, Ok(()));
        assert_eq!(format!("{Success}"), "Success");

        let res = Ok::<_, TestError>(());
        let info = ResultSignature::from(&res);
        assert_eq!(info.signature(), "Success");

        let res = Err::<(), _>(TestError::Service("409 Error"));
        let info = ResultSignature::from(&res);
        assert_eq!(info.signature(), "Service-Internal");
    }
}
