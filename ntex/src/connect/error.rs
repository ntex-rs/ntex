use std::io;

use derive_more::{Display, From};

#[derive(Debug, From, Display)]
pub enum ConnectError {
    /// Failed to resolve the hostname
    #[from(ignore)]
    #[display(fmt = "Failed resolving hostname: {}", _0)]
    Resolver(io::Error),

    /// No dns records
    #[display(fmt = "No dns records found for the input")]
    NoRecords,

    /// Invalid input
    InvalidInput,

    /// Unresolved host name
    #[display(fmt = "Connector received `Connect` method with unresolved host")]
    Unresolved,

    /// Connection io error
    #[display(fmt = "{}", _0)]
    Io(io::Error),
}

impl Clone for ConnectError {
    fn clone(&self) -> Self {
        match self {
            ConnectError::Resolver(err) => {
                ConnectError::Resolver(io::Error::new(err.kind(), format!("{}", err)))
            }
            ConnectError::NoRecords => ConnectError::NoRecords,
            ConnectError::InvalidInput => ConnectError::InvalidInput,
            ConnectError::Unresolved => ConnectError::Unresolved,
            ConnectError::Io(err) => {
                ConnectError::Io(io::Error::new(err.kind(), format!("{}", err)))
            }
        }
    }
}
