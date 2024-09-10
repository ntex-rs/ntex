//! Utilities for abstructing io streams
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]
#![allow(async_fn_in_trait)]

use std::{
    any::Any, any::TypeId, fmt, io as sio, io::Error as IoError, task::Context, task::Poll,
};

pub mod testing;
pub mod types;

mod buf;
mod dispatcher;
mod filter;
mod flags;
mod framed;
mod io;
mod ioref;
mod seal;
mod tasks;
mod timer;
mod utils;

use ntex_bytes::BytesVec;
use ntex_codec::{Decoder, Encoder};
use ntex_util::time::Millis;

pub use self::buf::{ReadBuf, WriteBuf};
pub use self::dispatcher::{Dispatcher, DispatcherConfig};
pub use self::filter::{Base, Filter, Layer};
pub use self::framed::Framed;
pub use self::io::{Io, IoRef, OnDisconnect};
pub use self::seal::{IoBoxed, Sealed};
pub use self::tasks::{ReadContext, WriteContext};
pub use self::timer::TimerHandle;
pub use self::utils::{seal, Decoded};

#[doc(hidden)]
pub use self::flags::Flags;

#[doc(hidden)]
/// Status for read task
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ReadStatus {
    Ready,
    Terminate,
}

#[doc(hidden)]
pub trait AsyncRead {
    async fn read(&mut self, buf: BytesVec) -> (BytesVec, sio::Result<usize>);
}

/// Status for write task
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum WriteStatus {
    /// Write task is clear to proceed with write operation
    Ready,
    /// Initiate timeout for normal write operations, shutdown connection after timeout
    Timeout(Millis),
    /// Initiate graceful io shutdown operation with timeout
    Shutdown(Millis),
    /// Immediately terminate connection
    Terminate,
}

#[allow(unused_variables)]
pub trait FilterLayer: fmt::Debug + 'static {
    /// Create buffers for this filter
    const BUFFERS: bool = true;

    #[inline]
    /// Check readiness for read operations
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<ReadStatus> {
        Poll::Ready(ReadStatus::Ready)
    }

    #[inline]
    /// Check readiness for write operations
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<WriteStatus> {
        Poll::Ready(WriteStatus::Ready)
    }

    /// Process read buffer
    ///
    /// Inner filter must process buffer before current.
    /// Returns number of new bytes.
    fn process_read_buf(&self, buf: &ReadBuf<'_>) -> sio::Result<usize>;

    /// Process write buffer
    fn process_write_buf(&self, buf: &WriteBuf<'_>) -> sio::Result<()>;

    #[inline]
    /// Query internal filter data
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>> {
        None
    }

    #[inline]
    /// Gracefully shutdown filter
    fn shutdown(&self, buf: &WriteBuf<'_>) -> sio::Result<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}

pub trait IoStream {
    fn start(self, _: ReadContext, _: WriteContext) -> Option<Box<dyn Handle>>;
}

pub trait Handle {
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>>;
}

/// Io status
#[derive(Debug)]
pub enum IoStatusUpdate {
    /// Keep-alive timeout occured
    KeepAlive,
    /// Write backpressure is enabled
    WriteBackpressure,
    /// Stop io stream handling
    Stop,
    /// Peer is disconnected
    PeerGone(Option<sio::Error>),
}

/// Recv error
#[derive(Debug)]
pub enum RecvError<U: Decoder> {
    /// Keep-alive timeout occured
    KeepAlive,
    /// Write backpressure is enabled
    WriteBackpressure,
    /// Stop io stream handling
    Stop,
    /// Unrecoverable frame decoding errors
    Decoder(U::Error),
    /// Peer is disconnected
    PeerGone(Option<sio::Error>),
}

/// Dispatcher item
pub enum DispatchItem<U: Encoder + Decoder> {
    Item(<U as Decoder>::Item),
    /// Write back-pressure enabled
    WBackPressureEnabled,
    /// Write back-pressure disabled
    WBackPressureDisabled,
    /// Keep alive timeout
    KeepAliveTimeout,
    /// Frame read timeout
    ReadTimeout,
    /// Decoder parse error
    DecoderError(<U as Decoder>::Error),
    /// Encoder parse error
    EncoderError(<U as Encoder>::Error),
    /// Socket is disconnected
    Disconnect(Option<IoError>),
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
            DispatchItem::ReadTimeout => {
                write!(fmt, "DispatchItem::ReadTimeout")
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

#[cfg(test)]
mod tests {
    use super::*;
    use ntex_codec::BytesCodec;
    use std::io;

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
        assert!(
            format!("{:?}", T::KeepAliveTimeout).contains("DispatchItem::KeepAliveTimeout")
        );
        assert!(format!("{:?}", T::ReadTimeout).contains("DispatchItem::ReadTimeout"));

        assert!(format!("{:?}", IoStatusUpdate::KeepAlive).contains("KeepAlive"));
        assert!(format!("{:?}", RecvError::<BytesCodec>::KeepAlive).contains("KeepAlive"));
    }
}
