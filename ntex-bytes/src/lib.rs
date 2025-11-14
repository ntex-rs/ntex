//! Provides abstractions for working with bytes.
//!
//! This is fork of [bytes crate](https://github.com/tokio-rs/bytes)
//!
//! The `ntex-bytes` crate provides an efficient byte buffer structure
//! ([`Bytes`](struct.Bytes.html)) and traits for working with buffer
//! implementations ([`Buf`], [`BufMut`]).
//!
//! [`Buf`]: trait.Buf.html
//! [`BufMut`]: trait.BufMut.html
//!
//! # `Bytes`
//!
//! `Bytes` is an efficient container for storing and operating on contiguous
//! slices of memory. It is intended for use primarily in networking code, but
//! could have applications elsewhere as well.
//!
//! `Bytes` values facilitate zero-copy network programming by allowing multiple
//! `Bytes` objects to point to the same underlying memory. This is managed by
//! using a reference count to track when the memory is no longer needed and can
//! be freed.
//!
//! A `Bytes` handle can be created directly from an existing byte store (such as `&[u8]`
//! or `Vec<u8>`), but usually a `BytesMut` is used first and written to. For
//! example:
//!
//! ```rust
//! use ntex_bytes::{BytesMut, BufMut};
//!
//! let mut buf = BytesMut::with_capacity(1024);
//! buf.put(&b"hello world"[..]);
//! buf.put_u16(1234);
//!
//! let a = buf.split();
//! assert_eq!(a, b"hello world\x04\xD2"[..]);
//!
//! buf.put(&b"goodbye world"[..]);
//!
//! let b = buf.split();
//! assert_eq!(b, b"goodbye world"[..]);
//!
//! assert_eq!(buf.capacity(), 998);
//! ```
//!
//! In the above example, only a single buffer of 1024 is allocated. The handles
//! `a` and `b` will share the underlying buffer and maintain indices tracking
//! the view into the buffer represented by the handle.
//!
//! See the [struct docs] for more details.
//!
//! [struct docs]: struct.Bytes.html

#![deny(warnings, missing_debug_implementations, rust_2018_idioms)]
#![doc(html_root_url = "https://docs.rs/ntex-bytes/")]

pub mod buf;
pub use crate::buf::{Buf, BufMut};

mod bvec;
mod bytes;
mod debug;
mod hex;
mod pool;
mod serde;
mod storage;
mod string;

pub use crate::bvec::BytesVec;
pub use crate::bytes::{Bytes, BytesMut};
pub use crate::string::ByteString;

#[doc(hidden)]
pub use crate::pool::{Pool, PoolId, PoolRef};
