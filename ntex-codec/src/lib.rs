//! Utilities for encoding and decoding frames.

use std::{fmt, io, rc::Rc};

use ntex_bytes::{BytePages, Bytes, BytesMut};

/// Trait of helper objects to write out messages as bytes.
pub trait Encoder {
    /// The type of items consumed by the `Encoder`
    type Item;

    /// The type of encoding errors.
    type Error: fmt::Debug;

    #[deprecated(since = "1.2.0", note = "Implement .encodev() method.")]
    /// Encodes a frame into the buffer provided.
    fn encode(&self, _: Self::Item, _: &mut BytesMut) -> Result<(), Self::Error> {
        panic!("Encoder::encodev() must be implemented")
    }

    /// Encodes a frame into the buffer provided.
    fn encodev(&self, item: Self::Item, dst: &mut BytePages) -> Result<(), Self::Error> {
        let mut buf = BytesMut::new();
        #[allow(deprecated)]
        self.encode(item, &mut buf)?;
        dst.append(buf.freeze());
        Ok(())
    }
}

/// Decoding of frames via buffers.
pub trait Decoder {
    /// The type of decoded frames.
    type Item: fmt::Debug;

    /// The type of unrecoverable frame decoding errors.
    ///
    /// If an individual message is ill-formed but can be ignored without
    /// interfering with the processing of future messages, it may be more
    /// useful to report the failure as an `Item`.
    type Error: fmt::Debug;

    /// Attempts to decode a frame from the provided buffer of bytes.
    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error>;
}

impl<T> Encoder for Rc<T>
where
    T: Encoder,
{
    type Item = T::Item;
    type Error = T::Error;

    #[allow(deprecated)]
    fn encode(&self, item: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        (**self).encode(item, dst)
    }

    fn encodev(&self, item: Self::Item, dst: &mut BytePages) -> Result<(), Self::Error> {
        (**self).encodev(item, dst)
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
    fn encodev(&self, item: Bytes, dst: &mut BytePages) -> Result<(), Self::Error> {
        dst.append(item);
        Ok(())
    }
}

impl Decoder for BytesCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.is_empty() {
            Ok(None)
        } else {
            Ok(Some(src.split_to(src.len())))
        }
    }
}
