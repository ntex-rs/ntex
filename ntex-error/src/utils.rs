use std::{convert::Infallible, error::Error as StdError, fmt};

use crate::{Error, ErrorDiagnostic, ResultType};

impl ErrorDiagnostic for Infallible {
    type Kind = ResultType;

    fn kind(&self) -> Self::Kind {
        unreachable!()
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
