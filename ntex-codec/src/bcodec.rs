use ntex_bytes::{Bytes, BytesMut};
use std::io;

use super::{Decoder, Encoder};

/// Bytes codec.
///
/// Reads/Writes chunks of bytes from a stream.
#[derive(Debug, Copy, Clone)]
pub struct BytesCodec;

impl Encoder for BytesCodec {
    type Item = Bytes;
    type Error = io::Error;

    #[inline]
    fn encode(&self, item: Bytes, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.extend_from_slice(&item[..]);
        Ok(())
    }
}

impl Decoder for BytesCodec {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.is_empty() {
            Ok(None)
        } else {
            let len = src.len();
            Ok(Some(src.split_to(len)))
        }
    }
}
