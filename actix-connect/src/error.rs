use std::io;

use derive_more::{Display, From};
use trust_dns_resolver::error::ResolveError;

#[derive(Debug, From, Display)]
pub enum ConnectError {
    /// Failed to resolve the hostname
    #[display(fmt = "Failed resolving hostname: {}", _0)]
    Resolver(ResolveError),

    /// No dns records
    #[display(fmt = "No dns records found for the input")]
    NoRecords,

    /// Invalid input
    InvalidInput,

    /// Unresolved host name
    #[display(fmt = "Connector received `Connect` method with unresolved host")]
    Unresolverd,

    /// Connection io error
    #[display(fmt = "{}", _0)]
    Io(io::Error),
}
