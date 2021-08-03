use std::pin::Pin;

use futures_core::FusedStream;

pub enum Backpressure {
    Disabled,
    Enabled,
    Blocked,
}

pub trait BackpressureStream: FusedStream<Item = Backpressure> {}

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
