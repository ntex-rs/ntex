//! Utilities for encoding and decoding frames.
//!
//! Contains adapters to go from streams of bytes, [`AsyncRead`] and
//! [`AsyncWrite`], to framed streams implementing [`Sink`] and [`Stream`].
//! Framed streams are also known as [transports].
//!
//! [`AsyncRead`]: #
//! [`AsyncWrite`]: #
#![deny(rust_2018_idioms, warnings)]

mod bcodec;
mod framed;

pub use self::bcodec::BytesCodec;
pub use self::framed::{Framed, FramedParts};

pub use tokio::io::{AsyncRead, AsyncWrite};
pub use tokio_util::codec::{Decoder, Encoder};
