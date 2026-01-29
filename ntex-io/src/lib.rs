//! Utilities for abstructing io streams
#![deny(rust_2018_idioms, unreachable_pub, missing_debug_implementations)]
#![allow(deprecated, async_fn_in_trait)]

use std::io::{Error as IoError, Result as IoResult};
use std::{any::Any, any::TypeId, fmt, task::Context, task::Poll};

pub mod cfg;
pub mod testing;
pub mod types;

mod buf;
mod filter;
mod flags;
mod framed;
mod io;
mod ioref;
mod macros;
mod seal;
mod tasks;
mod timer;
mod utils;

use ntex_codec::Decoder;

pub use self::buf::{FilterCtx, ReadBuf, WriteBuf};
pub use self::cfg::IoConfig;
pub use self::filter::{Base, Filter, FilterReadStatus, Layer};
pub use self::framed::Framed;
pub use self::io::{Io, IoRef, OnDisconnect};
pub use self::seal::{IoBoxed, Sealed};
pub use self::tasks::IoContext;
pub use self::timer::TimerHandle;
pub use self::utils::{Decoded, seal};

#[doc(hidden)]
pub use self::flags::Flags;

/// Filter ready state
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Readiness {
    /// Io task is clear to proceed with io operations
    Ready,
    /// Initiate graceful io shutdown operation
    Shutdown,
    /// Immediately terminate connection
    Terminate,
}

impl Readiness {
    /// Merge two Readiness values
    pub fn merge(val1: Poll<Readiness>, val2: Poll<Readiness>) -> Poll<Readiness> {
        match val1 {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Readiness::Ready) => val2,
            Poll::Ready(Readiness::Terminate) => Poll::Ready(Readiness::Terminate),
            Poll::Ready(Readiness::Shutdown) => {
                if val2 == Poll::Ready(Readiness::Terminate) {
                    Poll::Ready(Readiness::Terminate)
                } else {
                    Poll::Ready(Readiness::Shutdown)
                }
            }
        }
    }
}

#[allow(unused_variables)]
pub trait FilterLayer: fmt::Debug + 'static {
    #[inline]
    /// Check readiness for read operations
    fn poll_read_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        Poll::Ready(Readiness::Ready)
    }

    #[inline]
    /// Check readiness for write operations
    fn poll_write_ready(&self, cx: &mut Context<'_>) -> Poll<Readiness> {
        Poll::Ready(Readiness::Ready)
    }

    /// Process read buffer
    ///
    /// Inner filter must process buffer before current.
    /// Returns number of new bytes.
    fn process_read_buf(&self, buf: &ReadBuf<'_>) -> IoResult<usize>;

    /// Process write buffer
    fn process_write_buf(&self, buf: &WriteBuf<'_>) -> IoResult<()>;

    #[inline]
    /// Query internal filter data
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>> {
        None
    }

    #[inline]
    /// Gracefully shutdown filter
    fn shutdown(&self, buf: &WriteBuf<'_>) -> IoResult<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}

pub trait IoStream {
    fn start(self, _: IoContext) -> Option<Box<dyn Handle>>;
}

pub trait Handle {
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>>;
}

/// Status for read task
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum IoTaskStatus {
    /// More io ops
    Io,
    /// Pause io task
    Pause,
    /// Stop io task
    Stop,
}

impl IoTaskStatus {
    #[inline]
    /// Ready for more io ops
    pub fn ready(self) -> bool {
        self == IoTaskStatus::Io
    }
}

/// Io status
#[derive(Debug)]
pub enum IoStatusUpdate {
    /// Keep-alive timeout occured
    KeepAlive,
    /// Write backpressure is enabled
    WriteBackpressure,
    /// Peer is disconnected
    PeerGone(Option<IoError>),
}

/// Recv error
pub enum RecvError<U: Decoder> {
    /// Keep-alive timeout occured
    KeepAlive,
    /// Write backpressure is enabled
    WriteBackpressure,
    /// Unrecoverable frame decoding errors
    Decoder(U::Error),
    /// Peer is disconnected
    PeerGone(Option<IoError>),
}

impl<U> fmt::Debug for RecvError<U>
where
    U: Decoder,
    <U as Decoder>::Error: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RecvError::KeepAlive => {
                write!(fmt, "RecvError::KeepAlive")
            }
            RecvError::WriteBackpressure => {
                write!(fmt, "RecvError::WriteBackpressure")
            }
            RecvError::Decoder(ref e) => {
                write!(fmt, "RecvError::Decoder({e:?})")
            }
            RecvError::PeerGone(ref e) => {
                write!(fmt, "RecvError::PeerGone({e:?})")
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
        assert!(format!("{:?}", IoStatusUpdate::KeepAlive).contains("KeepAlive"));
        assert!(format!("{:?}", RecvError::<BytesCodec>::KeepAlive).contains("KeepAlive"));
        assert!(
            format!("{:?}", RecvError::<BytesCodec>::WriteBackpressure)
                .contains("WriteBackpressure")
        );
        assert!(
            format!(
                "{:?}",
                RecvError::<BytesCodec>::Decoder(io::Error::other("err"))
            )
            .contains("RecvError::Decoder")
        );
        assert!(
            format!(
                "{:?}",
                RecvError::<BytesCodec>::PeerGone(Some(io::Error::other("err")))
            )
            .contains("RecvError::PeerGone")
        );
    }
}
