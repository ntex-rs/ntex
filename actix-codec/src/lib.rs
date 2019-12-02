//! Utilities for encoding and decoding frames.
//!
//! Contains adapters to go from streams of bytes, [`AsyncRead`] and
//! [`AsyncWrite`], to framed streams implementing [`Sink`] and [`Stream`].
//! Framed streams are also known as [transports].
//!
//! [`AsyncRead`]: #
//! [`AsyncWrite`]: #
//! [`Sink`]: #
//! [`Stream`]: #
//! [transports]: #
#![deny(rust_2018_idioms, warnings)]
#![allow(clippy::type_complexity)]

mod bcodec;
mod framed;

pub use self::bcodec::BytesCodec;
pub use self::framed::{Framed, FramedParts};

pub use tokio_codec::{Decoder, Encoder};
pub use tokio_io::{AsyncRead, AsyncWrite};
