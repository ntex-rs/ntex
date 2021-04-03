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
