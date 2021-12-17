#![deny(rust_2018_idioms, warnings)]
//! Utilities for encoding and decoding frames.

mod bcodec;
mod decoder;
mod encoder;

pub use self::bcodec::BytesCodec;
pub use self::decoder::Decoder;
pub use self::encoder::Encoder;
