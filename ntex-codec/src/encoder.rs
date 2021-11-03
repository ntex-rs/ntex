use ntex_bytes::{BufferSource, BytesPool};
use std::future::Future;

/// Trait of helper objects to write out messages as bytes.
pub trait Encoder<P = BytesPool> where P: BufferSource {
    /// The type of items consumed by the `Encoder`
    type Item;

    /// The type of encoding errors.
    type Error: std::fmt::Debug;

    /// The type of future returned by `Encoder::encode`.
    type EncodeFuture: Future<Output = Result<(), Self::Error>>;

    /// Encodes a frame into the provided accumulator.
    fn encode(
        &mut self,
        item: Self::Item,
        pool: &P,
        dst: &mut P::Accumulator,
    ) -> Self::EncodeFuture;
}
