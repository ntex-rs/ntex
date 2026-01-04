#![deny(rust_2018_idioms, warnings)]
//! Utilities for encoding and decoding frames.

use std::{io, rc::Rc};

use ntex_bytes::{Bytes, BytesMut, BytesVec};

/// Trait of helper objects to write out messages as bytes.
pub trait Encoder {
    /// The type of items consumed by the `Encoder`
    type Item;

    /// The type of encoding errors.
    type Error: std::fmt::Debug;

    /// Encodes a frame into the buffer provided.
    fn encode(&self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error>;

    /// Encodes a frame into the buffer provided.
    fn encode_vec(&self, item: Self::Item, dst: &mut BytesVec) -> Result<(), Self::Error> {
        dst.with_bytes_mut(|dst| self.encode(item, dst))
    }
}

/// Decoding of frames via buffers.
pub trait Decoder {
    /// The type of decoded frames.
    type Item;

    /// The type of unrecoverable frame decoding errors.
    ///
    /// If an individual message is ill-formed but can be ignored without
    /// interfering with the processing of future messages, it may be more
    /// useful to report the failure as an `Item`.
    type Error: std::fmt::Debug;

    /// Attempts to decode a frame from the provided buffer of bytes.
    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error>;

    /// Attempts to decode a frame from the provided buffer of bytes.
    fn decode_vec(&self, src: &mut BytesVec) -> Result<Option<Self::Item>, Self::Error> {
        src.with_bytes_mut(|src| self.decode(src))
    }
}

impl<T> Encoder for Rc<T>
where
    T: Encoder,
{
    type Item = T::Item;
    type Error = T::Error;

    fn encode(&self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        (**self).encode(item, dst)
    }
}

impl<T> Decoder for Rc<T>
where
    T: Decoder,
{
    type Item = T::Item;
    type Error = T::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        (**self).decode(src)
    }
}

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
            Ok(Some(src.split_to(src.len())))
        }
    }
}
