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
    /// The transport must be closed gracefully.
    ///
    /// The I/O task must close both directions of the connection and then
    /// release it. For a socket this is `shutdown(SHUT_RDWR)` followed by
    /// `close()`. Any operation still in flight should be canceled.
    ///
    /// Buffered output has already been drained before this is reported, so
    /// there is nothing left to flush.
    Close,
    /// The transport must be released immediately.
    ///
    /// This is reported only for an explicit force close through
    /// [`IoRef::terminate`](crate::IoRef::terminate), so whatever is still
    /// buffered is discarded on purpose. The I/O task must not perform a
    /// graceful close: no receive queue drain and no `shutdown(SHUT_RDWR)`,
    /// just release the connection. That keeps an aborted stream
    /// distinguishable from one that ended normally, instead of terminating a
    /// truncated response with a clean `FIN`.
    ///
    /// A connection that ends because of an I/O failure, a filter failure or an
    /// expired shutdown deadline reports [`Close`](Self::Close) instead: the
    /// transport is gone or unusable, so there is nothing to gain from
    /// aborting it.
    Terminate,
}

impl Readiness {
    /// Merges two readiness states without regard to argument order.
    ///
    /// `Terminate` overrides every other state, `Close` overrides `Pending` and
    /// `Ready`, and `Pending` overrides `Ready`.
    pub fn merge(val1: Poll<Readiness>, val2: Poll<Readiness>) -> Poll<Readiness> {
        match (val1, val2) {
            (Poll::Ready(Readiness::Terminate), _) | (_, Poll::Ready(Readiness::Terminate)) => {
                Poll::Ready(Readiness::Terminate)
            }
            (Poll::Ready(Readiness::Close), _) | (_, Poll::Ready(Readiness::Close)) => {
                Poll::Ready(Readiness::Close)
            }
            (Poll::Pending, _) | (_, Poll::Pending) => Poll::Pending,
            (Poll::Ready(Readiness::Ready), Poll::Ready(Readiness::Ready)) => {
                Poll::Ready(Readiness::Ready)
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
    ///
    /// This is also called once after clean transport read EOF, with
    /// [`IoRef::is_read_eof`] returning `true`.
    fn process_read_buf(&self, buf: &FilterBuf<'_>) -> IoResult<()>;

    /// Processes outgoing data from the application-facing source buffer into
    /// the transport-facing destination buffer.
    fn process_write_buf(&self, buf: &FilterBuf<'_>) -> IoResult<()>;

    /// Performs graceful filter shutdown.
    ///
    /// Returning `Poll::Pending` keeps the filter active and causes shutdown to
    /// be polled again after the I/O task is notified. A ready result allows
    /// shutdown to continue toward the transport.
    ///
    /// A filter that waits for input from the peer must check
    /// [`IoRef::is_read_eof`] and return a ready result once it is set: after a
    /// clean read EOF no further input can arrive, so pending forever would
    /// only stall the close until the shutdown timeout expires. The runtime
    /// also ends the shutdown phase itself in that case, but it cannot know
    /// whether the filter considers the shutdown complete.
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
/// block.
pub trait Handle {
    /// Returns type-indexed transport information.
    fn query(&self, _: TypeId) -> Option<Box<dyn Any>> {
        None
    }

    #[inline]
    /// Requests that the transport start or resume a write operation.
    fn write(&self, _: &IoContext) {}
}

/// Current status of the I/O state.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum IoTaskStatus {
    /// Work remains, the task should perform another I/O operation.
    ///
    /// This reports that the connection still has work to do, not that the
    /// transport can accept or supply bytes right now. A task must re-arm
    /// transport readiness before the next operation. On the write side it is
    /// returned whenever output is still buffered, including after an attempt
    /// that made no progress.
    Io,
    /// Pause the task until the context wakes it.
    Pause,
    /// Stop the task and release its transport resources.
    Stop,
}

/// I/O status update events.
#[derive(Debug)]
pub enum IoStatusUpdate {
    /// The dispatcher timer has expired.
    Timeout,
    /// Write backpressure is currently active.
    WriteBackpressure,
    /// The connection is no longer usable.
    ///
    /// Reported once the connection has closed, whether because the peer
    /// disconnected, the transport failed, or the shutdown was started
    /// locally with [`IoRef::close`](crate::IoRef::close) or
    /// [`IoRef::terminate`](crate::IoRef::terminate). Carries the transport
    /// error when the connection ended because of one, and `None` when it
    /// closed cleanly.
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
    /// The connection is no longer usable.
    ///
    /// Reported once the connection has closed, whether because the peer
    /// disconnected, the transport failed, or the shutdown was started
    /// locally with [`IoRef::close`](crate::IoRef::close) or
    /// [`IoRef::terminate`](crate::IoRef::terminate). Carries the transport
    /// error when the connection ended because of one, and `None` when it
    /// closed cleanly.
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
        assert!(format!("{:?}", IoStatusUpdate::Timeout).contains("Timeout"));
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

    #[test]
    fn readiness_merge() {
        let states = [
            Poll::Pending,
            Poll::Ready(Readiness::Ready),
            Poll::Ready(Readiness::Close),
            Poll::Ready(Readiness::Terminate),
        ];

        for val1 in states {
            for val2 in states {
                assert_eq!(Readiness::merge(val1, val2), Readiness::merge(val2, val1));
            }
        }

        assert_eq!(
            Readiness::merge(Poll::Pending, Poll::Ready(Readiness::Ready)),
            Poll::Pending
        );
        assert_eq!(
            Readiness::merge(Poll::Pending, Poll::Ready(Readiness::Close)),
            Poll::Ready(Readiness::Close)
        );
        assert_eq!(
            Readiness::merge(Poll::Ready(Readiness::Ready), Poll::Ready(Readiness::Close)),
            Poll::Ready(Readiness::Close)
        );
        assert_eq!(
            Readiness::merge(Poll::Pending, Poll::Ready(Readiness::Terminate)),
            Poll::Ready(Readiness::Terminate)
        );
        assert_eq!(
            Readiness::merge(
                Poll::Ready(Readiness::Close),
                Poll::Ready(Readiness::Terminate)
            ),
            Poll::Ready(Readiness::Terminate)
        );
    }
}
