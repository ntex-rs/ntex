use std::fmt;

/// Error returned by x509 operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Input is not valid DER encoded X.509 data.
    InvalidDer(String),
    /// Input is not valid PEM.
    InvalidPem(String),
    /// PKCS#12 / key material could not be loaded.
    InvalidIdentity(String),
    /// Requested item (certificate in a store) was not found.
    NotFound(String),
    /// Crypto backend failure.
    Backend(String),
    /// Operation is not supported (by this platform or for this key type).
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidDer(e) => write!(f, "Invalid DER certificate: {e}"),
            Error::InvalidPem(e) => write!(f, "Invalid PEM certificate: {e}"),
            Error::InvalidIdentity(e) => write!(f, "Invalid identity: {e}"),
            Error::NotFound(e) => write!(f, "Not found: {e}"),
            Error::Backend(e) => write!(f, "Crypto backend error: {e}"),
            Error::Unsupported(e) => write!(f, "Unsupported: {e}"),
        }
    }
}

impl std::error::Error for Error {}
