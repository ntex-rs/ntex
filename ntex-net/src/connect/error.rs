use std::io;

use ntex_error::ErrorDiagnostic;

#[derive(thiserror::Error, Debug, Copy, Clone)]
pub enum ConnectServiceError {
    /// Cannot create connect service
    #[error("Cannot create connect service")]
    CannotCreateService,
}

#[derive(thiserror::Error, Debug)]
pub enum ConnectError {
    /// Failed to resolve the hostname
    #[error("Failed resolving hostname: {0}")]
    Resolver(io::Error),

    /// No dns records
    #[error("No dns records found for the input")]
    NoRecords,

    /// Invalid input
    #[error("Invalid input")]
    InvalidInput,

    /// Unresolved host name
    #[error("Connector received `Connect` method with unresolved host")]
    Unresolved,

    /// Connection io error
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl Clone for ConnectError {
    fn clone(&self) -> Self {
        match self {
            ConnectError::Resolver(err) => {
                ConnectError::Resolver(io::Error::new(err.kind(), format!("{err}")))
            }
            ConnectError::NoRecords => ConnectError::NoRecords,
            ConnectError::InvalidInput => ConnectError::InvalidInput,
            ConnectError::Unresolved => ConnectError::Unresolved,
            ConnectError::Io(err) => {
                ConnectError::Io(io::Error::new(err.kind(), format!("{err}")))
            }
        }
    }
}

impl From<ConnectServiceError> for io::Error {
    fn from(err: ConnectServiceError) -> io::Error {
        io::Error::other(err)
    }
}

impl ErrorDiagnostic for ConnectError {
    fn signature(&self) -> &'static str {
        match self {
            ConnectError::InvalidInput => "ntex-connect-InvalidInput",
            ConnectError::Resolver(_) => "ntex-connect-Resolver",
            ConnectError::NoRecords => "ntex-connect-NoRecords",
            ConnectError::Unresolved => "ntex-connect-Unresolved",
            ConnectError::Io(err) => err.signature(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::redundant_clone)]
    fn connect_error_clone() {
        let _ = ConnectError::Resolver(io::Error::other("test")).clone();
        let _ = ConnectError::NoRecords.clone();
        let _ = ConnectError::InvalidInput.clone();
        let _ = ConnectError::Unresolved.clone();
        let _ = ConnectError::Io(io::Error::other("test")).clone();
    }

    #[test]
    fn error_diagnostic() {
        let err = ConnectError::InvalidInput;
        assert_eq!(err.signature(), "ntex-connect-InvalidInput");

        let err = ConnectError::Resolver(io::Error::other("test"));
        assert_eq!(err.signature(), "ntex-connect-Resolver");

        let err = ConnectError::NoRecords;
        assert_eq!(err.signature(), "ntex-connect-NoRecords");

        let err = ConnectError::Unresolved;
        assert_eq!(err.signature(), "ntex-connect-Unresolved");

        let err = ConnectError::Io(io::Error::new(io::ErrorKind::InvalidInput, "test"));
        assert_eq!(err.signature(), "io-InvalidInput");

        let err = ConnectError::Io(io::Error::other("test"));
        assert_eq!(err.signature(), "io-Error");
    }
}
