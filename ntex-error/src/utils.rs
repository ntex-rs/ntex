use std::{convert::Infallible, error::Error as StdError, fmt, io};

use crate::{Error, ErrorDiagnostic, ResultType};

impl ErrorDiagnostic for Infallible {
    type Kind = ResultType;

    fn kind(&self) -> Self::Kind {
        unreachable!()
    }
}

impl ErrorDiagnostic for io::Error {
    type Kind = ResultType;

    fn kind(&self) -> Self::Kind {
        match self.kind() {
            io::ErrorKind::InvalidData
            | io::ErrorKind::Unsupported
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut => ResultType::ClientError,
            _ => ResultType::ServiceError,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Success;

impl StdError for Success {}

impl ErrorDiagnostic for Success {
    type Kind = ResultType;

    fn kind(&self) -> Self::Kind {
        ResultType::Success
    }
}

impl fmt::Display for Success {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Success")
    }
}

/// Execute future and set error service.
///
/// Sets service if it not set
pub async fn with_service<F, T, E>(svc: &'static str, fut: F) -> F::Output
where
    F: Future<Output = Result<T, Error<E>>>,
    E: ErrorDiagnostic + Clone,
{
    fut.await.map_err(|err: Error<E>| {
        if err.service().is_none() {
            err.set_service(svc)
        } else {
            err
        }
    })
}
