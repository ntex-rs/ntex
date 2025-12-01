use std::io;

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
}
