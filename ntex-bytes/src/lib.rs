//! Provides abstractions for working with bytes.
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
//! A `Bytes` handle can be created directly from an existing `BytesMut` store
//! is used first and written to. For example:
//!
//! ```rust
//! use ntex_bytes::{BytesMut, BufMut};
//!
//! let mut buf = BytesMut::with_capacity(1024);
//! buf.put(&b"hello world"[..]);
//! buf.put_u16(1234);
//!
//! let a = buf.take();
//! assert_eq!(a, b"hello world\x04\xD2"[..]);
//!
//! buf.put(&b"goodbye world"[..]);
//!
//! let b = buf.take();
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
//!
#![doc(html_root_url = "https://docs.rs/ntex-bytes/")]
#![deny(clippy::pedantic)]
#![allow(
    unsafe_op_in_unsafe_fn,
    clippy::must_use_candidate,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation
)]

extern crate alloc;

pub mod buf;
pub use crate::buf::{Buf, BufMut};

mod bvec;
mod bytes;
mod debug;
mod hex;
mod pages;
mod serde;
mod storage;
mod string;

pub use crate::bvec::BytesMut;
pub use crate::bytes::Bytes;
pub use crate::pages::{BytePage, BytePages};
pub use crate::string::ByteString;

#[doc(hidden)]
pub use crate::storage::METADATA_SIZE;

#[doc(hidden)]
#[deprecated]
pub type BytesVec = BytesMut;

#[doc(hidden)]
pub mod info {
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub struct Info {
        pub id: usize,
        pub refs: u32,
        pub kind: Kind,
        pub capacity: usize,
    }

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub enum Kind {
        Inline,
        Static,
        Vec,
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum BytePageSize {
    Size4 = 0,
    Size8 = 1,
    #[default]
    Size16 = 2,
    Size24 = 3,
    Size32 = 4,
    Size48 = 5,
    Size64 = 6,
    Unset = 7,
}

impl BytePageSize {
    pub const fn capacity(self) -> usize {
        match self {
            BytePageSize::Size4 => 4 * 1024,
            BytePageSize::Size8 => 8 * 1024,
            BytePageSize::Size16 => 16 * 1024,
            BytePageSize::Size24 => 24 * 1024,
            BytePageSize::Size32 => 32 * 1024,
            BytePageSize::Size48 => 48 * 1024,
            BytePageSize::Size64 | BytePageSize::Unset => 64 * 1024,
        }
    }

    pub const fn half_capacity(self) -> usize {
        match self {
            BytePageSize::Size4 => 2 * 1024,
            BytePageSize::Size8 => 4 * 1024,
            BytePageSize::Size16 => 8 * 1024,
            BytePageSize::Size24 => 12 * 1024,
            BytePageSize::Size32
            | BytePageSize::Size48
            | BytePageSize::Size64
            | BytePageSize::Unset => 16 * 1024,
        }
    }
}

/// Set pages cache size
///
/// Size is set for current thread
pub fn set_pages_cache(size: usize) {
    self::storage::set_pages_cache(size);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size() {
        assert_eq!(BytePageSize::Size4.capacity(), 4 * 1024);
        assert_eq!(BytePageSize::Size8.capacity(), 8 * 1024);
        assert_eq!(BytePageSize::Size16.capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size24.capacity(), 24 * 1024);
        assert_eq!(BytePageSize::Size32.capacity(), 32 * 1024);
        assert_eq!(BytePageSize::Size48.capacity(), 48 * 1024);
        assert_eq!(BytePageSize::Size64.capacity(), 64 * 1024);
        assert_eq!(BytePageSize::Unset.capacity(), 64 * 1024);
        assert_eq!(BytePageSize::Size4.half_capacity(), 2 * 1024);
        assert_eq!(BytePageSize::Size8.half_capacity(), 4 * 1024);
        assert_eq!(BytePageSize::Size16.half_capacity(), 8 * 1024);
        assert_eq!(BytePageSize::Size24.half_capacity(), 12 * 1024);
        assert_eq!(BytePageSize::Size32.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size48.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size64.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Unset.half_capacity(), 16 * 1024);
    }
}
