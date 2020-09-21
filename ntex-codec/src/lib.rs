//! Utilities for encoding and decoding frames.
//!
//! Contains adapters to go from streams of bytes, [`AsyncRead`] and
//! [`AsyncWrite`], to framed streams implementing [`Sink`] and [`Stream`].
//! Framed streams are also known as [transports].
//!
//! [`AsyncRead`]: #
//! [`AsyncWrite`]: #
#![deny(rust_2018_idioms, warnings)]

use either::Either;
use std::io;
use std::task::{Context, Poll};

mod bcodec;
mod decoder;
mod encoder;
mod framed;

pub use self::bcodec::BytesCodec;
pub use self::decoder::Decoder;
pub use self::encoder::Encoder;
pub use self::framed::{Framed, FramedParts};

pub use tokio::io::{AsyncRead, AsyncWrite};

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BufStatus {
    /// Buffer is empty
    Empty,
    /// Buffer is ready for write operation
    Ready,
    /// Buffer is full
    Full,
}

/// A unified interface to an underlying I/O object, using
/// the `Encoder` and `Decoder` traits to encode and decode frames.
pub trait IoFramed<U: Decoder + Encoder>: Unpin {
    /// Write buffer status
    fn buf_status(&self) -> BufStatus;

    /// Try to read underlying I/O stream and decode item.
    fn read(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        Option<Result<<U as Decoder>::Item, Either<<U as Decoder>::Error, io::Error>>>,
    >;

    /// Serialize item and write to the inner buffer
    fn write(&mut self, item: <U as Encoder>::Item)
        -> Result<(), <U as Encoder>::Error>;

    /// Flush write buffer to underlying I/O stream.
    fn flush(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>>;

    /// Flush write buffer and shutdown underlying I/O stream.
    ///
    /// Close method shutdown write side of a io object and
    /// then reads until disconnect or error, high level code must use
    /// timeout for close operation.
    fn close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>>;
}
