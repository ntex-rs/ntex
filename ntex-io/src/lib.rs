//! Utilities for abstructing io streams
#![deny(clippy::pedantic)]
#![allow(
    clippy::missing_fields_in_debug,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate
)]
use std::io::{Error as IoError, Result as IoResult};
use std::{any::Any, any::TypeId, fmt, task::Poll};

pub mod cfg;
pub mod testing;
pub mod types;

mod buf;
mod ctx;
mod filter;
mod filterptr;
mod flags;
mod framed;
mod io;
mod ioref;
mod macros;
mod ops;
mod seal;
mod utils;

use ntex_codec::Decoder;

pub use self::buf::{FilterBuf, FilterCtx};
pub use self::cfg::IoConfig;
pub use self::ctx::IoContext;
pub use self::filter::{Base, Filter, Layer};
pub use self::framed::Framed;
pub use self::io::{Io, IoRef, OnDisconnect};
pub use self::ops::{Id, TimerHandle};
pub use self::seal::{IoBoxed, Sealed};
pub use self::utils::{Decoded, seal};

#[doc(hidden)]
pub use self::flags::Flags;

/// Filter readiness state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Readiness {
    /// The I/O task may proceed with I/O operations.
    Ready,
    /// Initiates a graceful I/O shutdown.
    Shutdown,
    /// Immediately terminates the I/O stream.
    Terminate,
}

impl Readiness {
    /// Merges two readiness states.
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
    /// Processes incoming read-buffer data.
    fn process_read_buf(&self, buf: &mut FilterBuf<'_>) -> IoResult<()>;

    /// Processes outgoing write-buffer data.
    fn process_write_buf(&self, buf: &mut FilterBuf<'_>) -> IoResult<()>;

    #[inline]
    /// Accesses internal filter information.
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>> {
        None
    }

    #[inline]
    /// Performs a graceful shutdown of the filter.
    fn shutdown(&self, buf: &mut FilterBuf<'_>) -> IoResult<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}

pub trait IoStream {
    fn start(self, _: IoContext) -> Box<dyn Handle>;
}

pub trait Handle {
    fn query(&self, _: TypeId) -> Option<Box<dyn Any>> {
        None
    }

    #[inline]
    /// Initiate io write operation
    fn write(&self, _: &IoContext) {}

    #[inline]
    /// Called when readiness changes
    fn notify(&self, ctx: &IoContext) {
        ctx.notify()
    }
}

/// Current status of the I/O state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum IoTaskStatus {
    /// Continue performing I/O operations.
    Io,
    /// Pause I/O processing temporarily.
    Pause,
    /// Stop the I/O task.
    Stop,
}

/// I/O status update events.
#[derive(Debug)]
pub enum IoStatusUpdate {
    /// Keep-alive timeout has occurred.
    KeepAlive,
    /// Write backpressure is currently active.
    WriteBackpressure,
    /// Peer has disconnected.
    PeerGone(Option<IoError>),
}

/// Errors that can occur while receiving data.
pub enum RecvError<U: Decoder> {
    /// A keep-alive timeout occurred.
    KeepAlive,
    /// Write backpressure is currently active.
    WriteBackpressure,
    /// Failed to decode an incoming frame.
    Decoder(U::Error),
    /// The peer has disconnected.
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
