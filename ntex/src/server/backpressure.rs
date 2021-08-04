use std::pin::Pin;

use futures_core::FusedStream;

/// Backpressure indicates the availability of a `Worker`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Backpressure {
    /// The `Worker` will operate normally.
    Disabled,
    /// The `Worker` should process queued connections but should also request
    /// that no more be queued.
    Enabled,
    /// The `Worker should stop processing connections and should also request
    /// that no more be queued.
    Blocked,
}

impl Default for Backpressure {
    fn default() -> Self {
        Self::Disabled
    }
}

/// Provides a stream to wake up the `Worker` when the `Backpressure` requirements
/// for the `Worker` have changed.
/// 
/// The stream should only yield changes to the `Backpressure` state. The stream
/// should never end. In the event of failure by termination or end-of-stream, no
/// backpressure will be applied for the rest of the `Worker`'s lifetime.
pub trait BackpressureStream: FusedStream<Item = Backpressure> {}

/// A type that can be sent to `Worker`s to create new `BackpressureStream`s.
pub trait BackpressureStreamFactory: Send + Clone + 'static {
    type Stream: BackpressureStream;

    fn create(&self) -> Self::Stream;
}

impl<F, T> BackpressureStreamFactory for F
where
    F: Fn() -> T + Send + Clone + 'static,
    T: BackpressureStream,
{
    type Stream = T;

    #[inline]
    fn create(&self) -> Self::Stream {
        (self)()
    }
}

pub(super) type BoxedBackpressureStream = Pin<Box<dyn BackpressureStream>>;

pub(super) trait InternalBackpressureStreamFactory: Send {
    fn clone_factory(&self) -> Box<dyn InternalBackpressureStreamFactory>;

    fn create(&self) -> BoxedBackpressureStream;
}

pub(super) type BoxedInternalBackpressureStreamFactory = Box<dyn InternalBackpressureStreamFactory>;

pub(super) struct BoxingBackpressureStreamFactory<F: BackpressureStreamFactory>(F);

impl<F> BoxingBackpressureStreamFactory<F>
where
    F: BackpressureStreamFactory,
{
    pub(super) fn new(factory: F) -> Self {
        Self(factory)
    }
}

impl<F> InternalBackpressureStreamFactory for BoxingBackpressureStreamFactory<F>
where
    F: BackpressureStreamFactory,
{
    fn clone_factory(&self) -> Box<dyn InternalBackpressureStreamFactory> {
        Box::new(Self(self.0.clone()))
    }

    fn create(&self) -> BoxedBackpressureStream {
        Box::pin(self.0.create())
    }
}
