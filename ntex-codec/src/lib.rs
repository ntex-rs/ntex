//! Traits for encoding and decoding frames.
//!
//! A codec turns a byte stream into frames and back. [`Decoder`] splits
//! incoming bytes into frames, [`Encoder`] serializes outgoing frames. The
//! `ntex-io` crate drives them: `Io::recv()` and `IoRef::decode()` run the
//! decoder on the read buffer, `Io::send()` and `IoRef::encode()` run the
//! encoder on the write buffer, and the `ntex-dispatcher` crate uses both to
//! connect a stream to a service.
//!
//! Both traits take `&self`, since a codec is shared between the reading and
//! writing sides. A codec that keeps state between calls, such as a parser
//! position or negotiated settings, has to use interior mutability, e.g.
//! [`Cell`](std::cell::Cell) or [`RefCell`](std::cell::RefCell).
//!
//! # Example
//!
//! A codec for newline-terminated lines:
//!
//! ```
//! use std::io;
//!
//! use ntex_bytes::{BytePages, Bytes, BytesMut};
//! use ntex_codec::{Decoder, Encoder};
//!
//! struct LineCodec;
//!
//! impl Decoder for LineCodec {
//!     type Item = Bytes;
//!     type Error = io::Error;
//!
//!     fn decode(&self, src: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
//!         match src.iter().position(|b| *b == b'\n') {
//!             // consume the line and its terminator
//!             Some(n) => Ok(Some(src.split_to(n + 1).slice(..n))),
//!             // incomplete line, wait for more input
//!             None => Ok(None),
//!         }
//!     }
//!
//!     fn decode_eof(&self, src: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
//!         match self.decode(src)? {
//!             Some(line) => Ok(Some(line)),
//!             // the last line has no terminator
//!             None if !src.is_empty() => Ok(Some(src.split_to(src.len()))),
//!             None => Ok(None),
//!         }
//!     }
//! }
//!
//! impl Encoder for LineCodec {
//!     type Item = Bytes;
//!     type Error = io::Error;
//!
//!     fn encode(&self, item: Bytes, dst: &mut BytePages) -> Result<(), io::Error> {
//!         if item.contains(&b'\n') {
//!             return Err(io::Error::new(io::ErrorKind::InvalidInput, "newline in line"));
//!         }
//!         dst.append(item);
//!         dst.extend_from_slice(b"\n");
//!         Ok(())
//!     }
//! }
//!
//! let mut src = BytesMut::from(&b"one\ntwo"[..]);
//! assert_eq!(LineCodec.decode(&mut src).unwrap().unwrap(), "one");
//! assert!(LineCodec.decode(&mut src).unwrap().is_none());
//! assert_eq!(LineCodec.decode_eof(&mut src).unwrap().unwrap(), "two");
//! assert!(LineCodec.decode_eof(&mut src).unwrap().is_none());
//! ```

use std::{fmt, io, rc::Rc};

use ntex_bytes::{BytePages, Bytes, BytesMut};

/// Serializes frames into bytes.
pub trait Encoder {
    /// The type of frames consumed by the encoder.
    type Item;

    /// The type of encoding errors.
    type Error: fmt::Debug;

    /// Encodes a frame and appends it to `dst`.
    ///
    /// `dst` is the write buffer, a list of pages. [`BytePages::append`] adds
    /// a `Bytes` value as a page of its own without copying it, while
    /// [`BytePages::extend_from_slice`] and the [`BufMut`](ntex_bytes::BufMut)
    /// methods copy into the pages.
    ///
    /// Output written to `dst` is not rolled back when this returns an
    /// error, it is sent like any other output. Validate the frame before
    /// writing it, so that an error does not leave a partial frame behind.
    fn encode(&self, item: Self::Item, dst: &mut BytePages) -> Result<(), Self::Error>;
}

/// Splits a byte stream into frames.
pub trait Decoder {
    /// The type of decoded frames.
    type Item: fmt::Debug;

    /// The type of unrecoverable frame decoding errors.
    ///
    /// If an individual message is ill-formed but can be ignored without
    /// interfering with the processing of future messages, it may be more
    /// useful to report the failure as an `Item`.
    type Error: fmt::Debug;

    /// Attempts to decode a frame from the buffered input.
    ///
    /// Returns `Ok(Some(item))` after removing exactly one frame's bytes from
    /// the front of `src`, and `Ok(None)` if `src` does not hold a complete
    /// frame yet. On `None` the partial frame must stay in `src`, it is
    /// passed in again together with the input that arrives next.
    ///
    /// This is called repeatedly while it returns frames, and may be called
    /// with an empty buffer.
    fn decode(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error>;

    /// Attempts to decode a frame once the transport reached a clean EOF.
    ///
    /// The peer closed its write half and no further input will arrive, so
    /// this is the decoder's chance to produce a frame from whatever is left
    /// in `src`, or to report a truncated one as an error. Once the transport
    /// is at EOF it is used instead of [`decode`](Self::decode) for every
    /// decode attempt, it may be called again after it returned `None`, and
    /// with an empty buffer.
    ///
    /// The default implementation calls [`decode`](Self::decode).
    fn decode_eof(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.decode(src)
    }
}

impl<T> Encoder for Rc<T>
where
    T: Encoder,
{
    type Item = T::Item;
    type Error = T::Error;

    fn encode(&self, item: Self::Item, dst: &mut BytePages) -> Result<(), Self::Error> {
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

    fn decode_eof(&self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        (**self).decode_eof(src)
    }
}

/// Passes bytes through unchanged.
///
/// Decoding returns everything buffered as one frame, and returns `None` only
/// when the buffer is empty. Encoding appends the `Bytes` value to the write
/// buffer as is, without copying it.
#[derive(Debug, Copy, Clone)]
pub struct BytesCodec;

impl Encoder for BytesCodec {
    type Item = Bytes;
    type Error = io::Error;

    #[inline]
    fn encode(&self, item: Bytes, dst: &mut BytePages) -> Result<(), Self::Error> {
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
