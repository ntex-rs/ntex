use std::{convert::Infallible, error::Error as StdError, fmt};

use crate::{ErrorDiagnostic, ResultType};

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
