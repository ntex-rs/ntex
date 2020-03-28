use std::fmt;

use crate::codec::{Decoder, Encoder};

/// Framed service errors
pub enum ServiceError<E, U: Encoder + Decoder> {
    /// Inner service error
    Service(E),
    /// Encoder parse error
    Encoder(<U as Encoder>::Error),
    /// Decoder parse error
    Decoder(<U as Decoder>::Error),
}

impl<E, U: Encoder + Decoder> From<E> for ServiceError<E, U> {
    fn from(err: E) -> Self {
        ServiceError::Service(err)
    }
}

impl<E, U: Encoder + Decoder> fmt::Debug for ServiceError<E, U>
where
    E: fmt::Debug,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ServiceError::Service(ref e) => {
                write!(fmt, "ServiceError::Service({:?})", e)
            }
            ServiceError::Encoder(ref e) => {
                write!(fmt, "ServiceError::Encoder({:?})", e)
            }
            ServiceError::Decoder(ref e) => {
                write!(fmt, "ServiceError::Encoder({:?})", e)
            }
        }
    }
}

impl<E, U: Encoder + Decoder> fmt::Display for ServiceError<E, U>
where
    E: fmt::Display,
    <U as Encoder>::Error: fmt::Debug,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ServiceError::Service(ref e) => write!(fmt, "{}", e),
            ServiceError::Encoder(ref e) => write!(fmt, "{:?}", e),
            ServiceError::Decoder(ref e) => write!(fmt, "{:?}", e),
        }
    }
}
