use ntex_bytes::{BufferAccumulator, BufferSink, BufferSource, Owned};
use std::future;
use std::io;

use super::{Decoder, Encoder};

/// Bytes codec.
///
/// Reads/Writes chunks of bytes from a stream.
#[derive(Debug, Copy, Clone)]
pub struct BytesCodec;

impl<P> Encoder<P> for BytesCodec where P: BufferSource {
    type Item = <P::Owned as Owned>::Shared;
    type Error = io::Error;
    type EncodeFuture = future::Ready<Result<(), Self::Error>>;

    #[inline]
    fn encode(&mut self, item: Self::Item, _pool: &P, dst: &mut P::Accumulator) -> Self::EncodeFuture {
        let result = dst.put_bytes(item).ok_or_else(|| io::Error::new(io::ErrorKind::Other, "accumulator is full"));
        future::ready(result)
    }
}

impl<P> Decoder<P> for BytesCodec where P: BufferSink {
    type Item = P::Owned;
    type Error = io::Error;

    fn decode(&self, src: &mut P::Owned) -> Result<Option<Self::Item>, Self::Error> {
        if src.filled_is_empty() {
            Ok(None)
        } else {
            let len = src.filled().len();
            Ok(Some(src.split_to(len)))
        }
    }
}
