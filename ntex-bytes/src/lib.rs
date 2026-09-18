//! Provides abstractions for working with bytes.
//!
//! The crate provides immutable [`Bytes`] and mutable [`BytesMut`] buffers,
//! UTF-8 [`ByteString`] values, paged buffers through [`BytePages`], and the
//! [`Buf`] and [`BufMut`] traits.
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
//! A common pattern is to write into a [`BytesMut`] and extract immutable
//! [`Bytes`] views:
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
//! In this example, a single 1,024-byte allocation is reused. The `a` and `b`
//! handles retain immutable views into that allocation, while `buf` continues
//! using its remaining capacity.
//!
//! See [`Bytes`] and [`BytesMut`] for details about sharing, splitting, and
//! allocation behavior.
//!
//! # Crate features
//!
//! - `simd` enables SIMD-accelerated UTF-8 validation.
//! - `overuse` enables diagnostic logging for unusually large page stacks.
#![doc(html_root_url = "https://docs.rs/ntex-bytes/")]
#![deny(clippy::pedantic)]
#![allow(
    unsafe_op_in_unsafe_fn,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::must_use_candidate,
    clippy::unnecessary_wraps
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
mod stvec;

mod stext;
mod stext_arc;

pub use crate::bvec::BytesMut;
pub use crate::bytes::Bytes;
pub use crate::pages::{BytePage, BytePages};
pub use crate::stext::{StorageExt, StorageExtStr, StorageVTable};
pub use crate::string::ByteString;

#[doc(hidden)]
pub use crate::stvec::METADATA_SIZE;

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
        StExt,
    }
}

/// Capacity category used when allocating [`BytePage`] storage.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum BytePageSize {
    /// A 4 KiB page.
    Size4 = 0,
    /// An 8 KiB page.
    Size8 = 1,
    /// A 16 KiB page.
    #[default]
    Size16 = 2,
    /// A 24 KiB page.
    Size24 = 3,
    /// A 32 KiB page.
    Size32 = 4,
    /// A 48 KiB page.
    Size48 = 5,
    /// A 64 KiB page.
    Size64 = 6,
    /// No fixed page category.
    Unset = 7,
}

impl BytePageSize {
    /// Returns the page capacity in bytes.
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

    /// Returns the recommended write-buffer threshold for this page size.
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

/// Sets the maximum number of cached page allocations for the current thread.
///
/// This setting affects only the thread on which it is called.
pub fn set_pages_cache(size: usize) {
    self::stvec::set_pages_cache(size);
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
