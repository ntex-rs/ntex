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
//! # Interoperability
//!
//! [`Bytes`] and [`BytesMut`] implement the [`Buf`](::bytes::Buf) trait of the
//! `bytes` crate, and [`BytesMut`] also implements its
//! [`BufMut`](::bytes::BufMut) trait. [`Bytes`] and [`ByteString`] implement
//! `serde`'s `Serialize` and `Deserialize`.
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

#[macro_use]
mod macros;

pub mod buf;
pub use crate::buf::{Buf, BufMut};

mod bvec;
mod bytes;
mod debug;
mod hex;
mod pages;
mod serde;
mod size;
mod storage;
mod string;
mod stvec;

mod stext;
mod stext_arc;

pub use crate::bvec::BytesMut;
pub use crate::bytes::Bytes;
pub use crate::pages::{BytePage, BytePages};
pub use crate::size::BytePageSize;
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

    /// Storage backing a [`BytePage`](crate::BytePage).
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub enum PageKind {
        /// Backed by `Bytes`, cloning shares the data.
        Bytes,
        /// Backed by `BytesMut` storage, cloning shares the data.
        Storage,
        /// Backed by `Vec<u8>`, cloning or splitting copies the data.
        Vec,
    }
}

/// Sets the maximum number of cached page allocations for every page size
/// on the current thread.
///
/// This setting affects only the thread on which it is called.
#[deprecated(
    since = "1.11.0",
    note = "the cache limit depends on the page size, use `set_page_cache_size()`"
)]
pub fn set_pages_cache(size: usize) {
    self::stvec::set_pages_cache(size);
}

/// Sets the maximum number of cached page allocations of page size `size` on
/// the current thread.
///
/// By default fewer pages are cached for larger page sizes:
///
/// | Page size | 4K  | 8K | 16K | 24K | 32K | 48K | 64K | 128K | 256K |
/// |-----------|-----|----|-----|-----|-----|-----|-----|------|------|
/// | Pages     | 128 | 64 | 64  | 32  | 16  | 8   | 16  | 2    | 1    |
///
/// Buffers of [`BytePageSize::Unset`] are never cached, the call does nothing
/// for it.
///
/// This setting affects only the thread on which it is called.
pub fn set_page_cache_size(size: BytePageSize, count: usize) {
    self::stvec::set_page_cache_size(size, count);
}
