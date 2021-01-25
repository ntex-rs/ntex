use std::{fmt, io};

mod dispatcher;
mod read;
mod state;
mod time;
mod write;

pub use self::dispatcher::Dispatcher;
pub use self::read::ReadTask;
pub use self::state::State;
pub use self::time::Timer;
pub use self::write::WriteTask;

use crate::codec::{Decoder, Encoder};

/// Framed transport item
pub enum DispatchItem<U: Encoder + Decoder> {
    Item(<U as Decoder>::Item),
    /// Write back-pressure enabled
    WBackPressureEnabled,
    /// Write back-pressure disabled
    WBackPressureDisabled,
    /// Keep alive timeout
    KeepAliveTimeout,
    /// Decoder parse error
    DecoderError(<U as Decoder>::Error),
    /// Encoder parse error
    EncoderError(<U as Encoder>::Error),
    /// Unexpected io error
    IoError(io::Error),
}

impl<U> fmt::Debug for DispatchItem<U>
where
    U: Encoder + Decoder,
    <U as Decoder>::Item: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DispatchItem::Item(ref item) => {
                write!(fmt, "DispatchItem::Item({:?})", item)
            }
            DispatchItem::WBackPressureEnabled => {
                write!(fmt, "DispatchItem::WBackPressureEnabled")
            }
            DispatchItem::WBackPressureDisabled => {
                write!(fmt, "DispatchItem::WBackPressureDisabled")
            }
            DispatchItem::KeepAliveTimeout => {
                write!(fmt, "DispatchItem::KeepAliveTimeout")
            }
            DispatchItem::EncoderError(ref e) => {
                write!(fmt, "DispatchItem::EncoderError({:?})", e)
            }
            DispatchItem::DecoderError(ref e) => {
                write!(fmt, "DispatchItem::DecoderError({:?})", e)
            }
            DispatchItem::IoError(ref e) => {
                write!(fmt, "DispatchItem::IoError({:?})", e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::codec::BytesCodec;

    #[test]
    fn test_fmt() {
        #[derive(Debug, Display)]
        type T = DispatchError<BytesCodec>;

        let err = T::Encoder(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{:?}", err).contains("DispatcherError::Encoder"));
        assert!(format!("{}", err).contains("Custom"));
        let err = T::Decoder(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{}", err).contains("Custom"));
        assert!(format!("{:?}", err).contains("DispatcherError::Decoder"));
        let err = T::IoError(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{}", err).contains("Custom"));
        assert!(format!("{:?}", err).contains("DispatcherError::IoError"));

        assert!(format!("{:?}", T::WBackPressureEnabled)
            .contains("DispatchItem::WBackPressureEnabled"));
        assert!(format!("{:?}", T::WBackPressureDisabled)
            .contains("DispatchItem::WBackPressureDisabled"));
        assert!(format!("{:?}", T::KeepAliveTimeout)
            .contains("DispatchItem::KeepAliveTimeout"));
    }
}
