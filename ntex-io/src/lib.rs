//! Asynchronous I/O abstractions for the ntex ecosystem.
//!
//! [`Io`] wraps an underlying [`IoStream`] and coordinates buffered reads,
//! writes, backpressure, timeouts, and shutdown. Protocol transforms can be
//! composed through [`Filter`] layers, while [`Framed`] combines an I/O stream
//! with an `ntex-codec` encoder and decoder.
//!
//! Use [`IoConfig`] to configure buffer thresholds and connection timeouts.
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
pub use self::utils::Decoded;

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

/// A processing layer that transforms an I/O stream's read and write buffers.
///
/// Read processing runs from the transport toward the application. Write and
/// shutdown processing run from the application toward the transport. Each
/// callback receives the buffers immediately before and after this layer.
///
/// Implementations must move or transform all bytes they consume. Bytes left
/// in a source buffer remain available to the layer on a later callback.
#[allow(unused_variables)]
pub trait FilterLayer: fmt::Debug + 'static {
    /// Returns type-indexed information exposed by this layer.
    ///
    /// Returning `None` allows the query to continue through the remaining
    /// filter chain.
    fn query(&self, id: TypeId) -> Option<Box<dyn Any>> {
        None
    }

    /// Processes incoming data from the transport-facing source buffer into
    /// the application-facing destination buffer.
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> IoResult<()>;

    /// Processes outgoing data from the application-facing source buffer into
    /// the transport-facing destination buffer.
    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> IoResult<()>;

    /// Performs one step of graceful filter shutdown.
    ///
    /// Returning `Poll::Pending` keeps the filter active and causes shutdown to
    /// be polled again after the I/O task is notified. A ready result allows
    /// shutdown to continue toward the transport.
    fn shutdown(&self, buf: &FilterBuf<'_>) -> IoResult<Poll<()>> {
        Ok(Poll::Ready(()))
    }
}

/// An underlying transport that can be managed by [`Io`].
///
/// [`start`](IoStream::start) is called exactly once when the transport is
/// wrapped in [`Io`]. The implementation must start its read and write tasks,
/// use the supplied [`IoContext`] to exchange buffers and readiness state, and
/// return a handle that remains valid for the connection's lifetime.
pub trait IoStream {
    /// Starts transport-specific I/O tasks and returns their control handle.
    fn start(self, _: IoContext) -> Box<dyn Handle>;
}

#[doc(hidden)]
/// Callbacks invoked around filter-chain processing.
pub trait IoCallbacks {
    /// Called before processing the read or write filter chain.
    fn before_processing(&self, io: &IoRef);

    /// Called after processing the read or write filter chain.
    fn after_processing(&self, io: &IoRef);
}

/// Control handle for transport-specific I/O tasks.
///
/// The handle is called synchronously by the connection state and must not
/// block. It can use [`IoContext::notify`] to wake a transport task after
/// readiness changes.
pub trait Handle {
    /// Returns type-indexed transport information.
    fn query(&self, _: TypeId) -> Option<Box<dyn Any>> {
        None
    }

    #[inline]
    /// Requests that the transport start or resume a write operation.
    fn write(&self, _: &IoContext) {}

    #[inline]
    /// Notifies the I/O context that readiness has changed.
    fn notify(&self, ctx: &IoContext) {
        ctx.notify();
    }
}

/// Current status of the I/O state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum IoTaskStatus {
    /// Continue performing I/O operations immediately.
    Io,
    /// Pause the task until the context or handle wakes it.
    Pause,
    /// Stop the task and release its transport resources.
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
