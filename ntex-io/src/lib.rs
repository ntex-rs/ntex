use std::{fmt, future::Future, io, task::Context, task::Poll};

pub mod testing;

mod dispatcher;
mod filter;
mod state;
mod tasks;
mod time;
mod utils;

#[cfg(feature = "tokio")]
mod tokio_impl;

use ntex_bytes::BytesMut;
use ntex_codec::{Decoder, Encoder};

pub use self::dispatcher::Dispatcher;
pub use self::state::{Io, IoRef, ReadRef, WriteRef};
pub use self::tasks::{ReadState, WriteState};
pub use self::time::Timer;

pub use self::utils::{from_iostream, into_boxed};

pub type IoBoxed = Io<Box<dyn Filter>>;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum WriteReadiness {
    Shutdown,
    Terminate,
}

pub trait ReadFilter {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>>;

    fn read_closed(&self, err: Option<io::Error>);

    fn get_read_buf(&self) -> Option<BytesMut>;

    fn release_read_buf(&self, buf: BytesMut, new_bytes: usize);
}

pub trait WriteFilter {
    fn poll_write_ready(&self, cx: &mut Context<'_>)
        -> Poll<Result<(), WriteReadiness>>;

    fn write_closed(&self, err: Option<io::Error>);

    fn get_write_buf(&self) -> Option<BytesMut>;

    fn release_write_buf(&self, buf: BytesMut);
}

pub trait Filter: ReadFilter + WriteFilter {}

pub trait FilterFactory<F: Filter>: Sized {
    type Filter: Filter;

    type Error: fmt::Debug;
    type Future: Future<Output = Result<Io<Self::Filter>, Self::Error>>;

    fn create(&self, st: Io<F>) -> Self::Future;
}

pub trait IoStream {
    fn start(self, _: ReadState, _: WriteState);
}

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
    use super::*;
    use ntex_codec::BytesCodec;

    #[test]
    fn test_fmt() {
        type T = DispatchItem<BytesCodec>;

        let err = T::EncoderError(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{:?}", err).contains("DispatchItem::Encoder"));
        let err = T::DecoderError(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{:?}", err).contains("DispatchItem::Decoder"));
        let err = T::IoError(io::Error::new(io::ErrorKind::Other, "err"));
        assert!(format!("{:?}", err).contains("DispatchItem::IoError"));

        assert!(format!("{:?}", T::WBackPressureEnabled)
            .contains("DispatchItem::WBackPressureEnabled"));
        assert!(format!("{:?}", T::WBackPressureDisabled)
            .contains("DispatchItem::WBackPressureDisabled"));
        assert!(format!("{:?}", T::KeepAliveTimeout)
            .contains("DispatchItem::KeepAliveTimeout"));
    }
}
