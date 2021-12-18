use std::{any::Any, any::TypeId, fmt, future::Future, io, task::Context, task::Poll};

pub mod testing;
pub mod types;

mod dispatcher;
mod filter;
mod state;
mod tasks;
mod time;
mod utils;

#[cfg(any(feature = "tokio-traits", feature = "tokio"))]
mod tokio_impl;
#[cfg(any(feature = "tokio"))]
mod tokio_rt;

use ntex_bytes::BytesMut;
use ntex_codec::{Decoder, Encoder};
use ntex_util::time::Millis;

pub use self::dispatcher::Dispatcher;
pub use self::filter::DefaultFilter;
pub use self::state::{Io, IoRef, OnDisconnect, ReadRef, WriteRef};
pub use self::tasks::{ReadContext, WriteContext};
pub use self::time::Timer;

pub use self::utils::{filter_factory, into_boxed};

pub type IoBoxed = Io<Box<dyn Filter>>;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum WriteReadiness {
    Timeout(Millis),
    Shutdown(Millis),
    Terminate,
}

pub trait ReadFilter {
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), ()>>;

    fn read_closed(&self, err: Option<io::Error>);

    fn get_read_buf(&self) -> Option<BytesMut>;

    fn release_read_buf(&self, buf: BytesMut, new_bytes: usize)
        -> Result<(), io::Error>;
}

pub trait WriteFilter {
    fn poll_write_ready(&self, cx: &mut Context<'_>)
        -> Poll<Result<(), WriteReadiness>>;

    fn write_closed(&self, err: Option<io::Error>);

    fn get_write_buf(&self) -> Option<BytesMut>;

    fn release_write_buf(&self, buf: BytesMut) -> Result<(), io::Error>;
}

pub trait Filter: ReadFilter + WriteFilter + 'static {
    fn shutdown(&self, st: &IoRef) -> Poll<Result<(), io::Error>>;

    fn query(&self, id: TypeId) -> Option<Box<dyn Any>>;
}

pub trait FilterFactory<F: Filter>: Sized {
    type Filter: Filter;

    type Error: fmt::Debug;
    type Future: Future<Output = Result<Io<Self::Filter>, Self::Error>>;

    fn create(self, st: Io<F>) -> Self::Future;
}

pub trait IoStream {
    fn start(self, _: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>>;
}

pub trait Handle {
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>>;
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
    /// Socket is disconnected
    Disconnect(Option<io::Error>),
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
            DispatchItem::Disconnect(ref e) => {
                write!(fmt, "DispatchItem::Disconnect({:?})", e)
            }
        }
    }
}

pub mod rt {
    //! async runtime helpers

    #[cfg(feature = "tokio")]
    pub use crate::tokio_rt::*;
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
        let err = T::Disconnect(Some(io::Error::new(io::ErrorKind::Other, "err")));
        assert!(format!("{:?}", err).contains("DispatchItem::Disconnect"));

        assert!(format!("{:?}", T::WBackPressureEnabled)
            .contains("DispatchItem::WBackPressureEnabled"));
        assert!(format!("{:?}", T::WBackPressureDisabled)
            .contains("DispatchItem::WBackPressureDisabled"));
        assert!(format!("{:?}", T::KeepAliveTimeout)
            .contains("DispatchItem::KeepAliveTimeout"));
    }
}
