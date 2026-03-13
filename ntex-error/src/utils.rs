use std::convert::Infallible;

use crate::{ErrorDiagnostic, ResultType};

impl ErrorDiagnostic for Infallible {
    type Kind = ResultType;

    fn kind(&self) -> Self::Kind {
        unreachable!()
    }
}
