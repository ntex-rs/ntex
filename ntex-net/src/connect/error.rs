use std::io;

use ntex_error::ErrorDiagnostic;

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
    #[error("Tcp connect")]
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
            ConnectError::Io(err) => ConnectError::Io(io::Error::new(err.kind(), format!("{err}"))),
        }
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

    /// STATEMENT: Cloning a `ConnectError` keeps its variant, and for `Resolver` and `Io`
    /// rebuilds the `io::Error` with the same kind and message.
    ///
    /// HOW:
    /// - Clone each unit variant and check the variant with `matches!`.
    /// - Clone `Resolver` and `Io` errors with distinct kinds; check the clone's variant,
    ///   `kind()` and message.
    #[test]
    #[allow(clippy::redundant_clone)]
    fn connect_error_clone() {
        assert!(matches!(
            ConnectError::NoRecords.clone(),
            ConnectError::NoRecords
        ));
        assert!(matches!(
            ConnectError::InvalidInput.clone(),
            ConnectError::InvalidInput
        ));
        assert!(matches!(
            ConnectError::Unresolved.clone(),
            ConnectError::Unresolved
        ));

        let err = ConnectError::Resolver(io::Error::new(io::ErrorKind::NotFound, "no host"));
        let ConnectError::Resolver(e) = err.clone() else {
            panic!("expected Resolver")
        };
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert_eq!(e.to_string(), "no host");

        let err = ConnectError::Io(io::Error::new(io::ErrorKind::ConnectionRefused, "refused"));
        let ConnectError::Io(e) = err.clone() else {
            panic!("expected Io")
        };
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(e.to_string(), "refused");
    }

    /// STATEMENT: `ConnectError` signatures name the connect error variant, or the I/O error
    /// kind for I/O errors.
    ///
    /// HOW:
    /// - Expect `ntex-connect-*` signatures for the input, resolver and record errors.
    /// - Expect `std-io-*` signatures with the kind for I/O errors.
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
        assert_eq!(err.signature(), "std-io-InvalidInput");

        let err = ConnectError::Io(io::Error::other("test"));
        assert_eq!(err.signature(), "std-io-Error");
    }
}
