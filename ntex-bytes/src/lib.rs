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

/// Capacity category used when allocating [`BytePage`] storage.
///
/// Buffers with a page size are returned to a per-thread cache of their
/// category when the last reference is dropped, see [`set_page_cache_size`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
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
    /// A 128 KiB page.
    Size128 = 7,
    /// A 256 KiB page.
    Size256 = 8,
    /// No fixed page category.
    ///
    /// Buffers of this category are sized on demand and never returned to
    /// the page cache. It cannot be used as the page size of
    /// [`BytePages`].
    Unset = 9,
}

/// Page categories in increasing order of size, `Unset` excluded.
const PAGE_SIZES: [BytePageSize; 9] = [
    BytePageSize::Size4,
    BytePageSize::Size8,
    BytePageSize::Size16,
    BytePageSize::Size24,
    BytePageSize::Size32,
    BytePageSize::Size48,
    BytePageSize::Size64,
    BytePageSize::Size128,
    BytePageSize::Size256,
];

impl BytePageSize {
    /// Returns the smallest page category with a [`capacity`](Self::capacity)
    /// of at least `capacity` bytes.
    ///
    /// Returns [`BytePageSize::Unset`] if `capacity` is larger than the
    /// capacity of the largest category.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytePageSize;
    ///
    /// assert_eq!(BytePageSize::for_capacity(100), BytePageSize::Size4);
    /// assert_eq!(BytePageSize::for_capacity(5000), BytePageSize::Size8);
    /// assert_eq!(BytePageSize::for_capacity(1024 * 1024), BytePageSize::Unset);
    /// ```
    pub const fn for_capacity(capacity: usize) -> BytePageSize {
        let mut i = 0;
        while i < PAGE_SIZES.len() {
            if capacity <= PAGE_SIZES[i].capacity() {
                return PAGE_SIZES[i];
            }
            i += 1;
        }
        BytePageSize::Unset
    }

    /// Returns the next larger page category.
    ///
    /// The largest category returns [`BytePageSize::Unset`], `Unset` returns
    /// itself.
    #[must_use]
    pub const fn next(self) -> BytePageSize {
        match self {
            BytePageSize::Size4 => BytePageSize::Size8,
            BytePageSize::Size8 => BytePageSize::Size16,
            BytePageSize::Size16 => BytePageSize::Size24,
            BytePageSize::Size24 => BytePageSize::Size32,
            BytePageSize::Size32 => BytePageSize::Size48,
            BytePageSize::Size48 => BytePageSize::Size64,
            BytePageSize::Size64 => BytePageSize::Size128,
            BytePageSize::Size128 => BytePageSize::Size256,
            BytePageSize::Size256 | BytePageSize::Unset => BytePageSize::Unset,
        }
    }

    /// Returns the next smaller page category.
    ///
    /// The smallest category returns itself, [`BytePageSize::Unset`] returns
    /// the largest category.
    #[must_use]
    pub const fn prev(self) -> BytePageSize {
        match self {
            BytePageSize::Size4 | BytePageSize::Size8 => BytePageSize::Size4,
            BytePageSize::Size16 => BytePageSize::Size8,
            BytePageSize::Size24 => BytePageSize::Size16,
            BytePageSize::Size32 => BytePageSize::Size24,
            BytePageSize::Size48 => BytePageSize::Size32,
            BytePageSize::Size64 => BytePageSize::Size48,
            BytePageSize::Size128 => BytePageSize::Size64,
            BytePageSize::Size256 => BytePageSize::Size128,
            BytePageSize::Unset => BytePageSize::Size256,
        }
    }

    /// Returns the page capacity in bytes.
    ///
    /// A page is allocated together with its header, the capacity is the
    /// category size minus the header, so the allocation is exactly the
    /// category size and fits the allocator's size classes.
    pub const fn capacity(self) -> usize {
        self.alloc_size() - stvec::METADATA_SIZE
    }

    const fn alloc_size(self) -> usize {
        match self {
            BytePageSize::Size4 => 4 * 1024,
            BytePageSize::Size8 => 8 * 1024,
            BytePageSize::Size16 => 16 * 1024,
            BytePageSize::Size24 => 24 * 1024,
            BytePageSize::Size32 => 32 * 1024,
            BytePageSize::Size48 => 48 * 1024,
            BytePageSize::Size64 | BytePageSize::Unset => 64 * 1024,
            BytePageSize::Size128 => 128 * 1024,
            BytePageSize::Size256 => 256 * 1024,
        }
    }

    /// Returns the recommended write-buffer threshold for this page size.
    ///
    /// This is half of the category size, but at most 16 KiB.
    pub const fn half_capacity(self) -> usize {
        match self {
            BytePageSize::Size4 => 2 * 1024,
            BytePageSize::Size8 => 4 * 1024,
            BytePageSize::Size16 => 8 * 1024,
            BytePageSize::Size24 => 12 * 1024,
            BytePageSize::Size32
            | BytePageSize::Size48
            | BytePageSize::Size64
            | BytePageSize::Size128
            | BytePageSize::Size256
            | BytePageSize::Unset => 16 * 1024,
        }
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
/// | Page size | 4K | 8K | 16K | 24K | 32K | 48K | 64K | 128K | 256K |
/// |-----------|----|----|-----|-----|-----|-----|-----|------|------|
/// | Pages     | 64 | 32 | 64  | 16  | 16  | 8   | 8   | 2    | 1    |
///
/// Buffers of [`BytePageSize::Unset`] are never cached, the call does nothing
/// for it.
///
/// This setting affects only the thread on which it is called.
pub fn set_page_cache_size(size: BytePageSize, count: usize) {
    self::stvec::set_page_cache_size(size, count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size() {
        const META: usize = stvec::METADATA_SIZE;
        assert_eq!(BytePageSize::Size4.capacity(), 4 * 1024 - META);
        assert_eq!(BytePageSize::Size8.capacity(), 8 * 1024 - META);
        assert_eq!(BytePageSize::Size16.capacity(), 16 * 1024 - META);
        assert_eq!(BytePageSize::Size24.capacity(), 24 * 1024 - META);
        assert_eq!(BytePageSize::Size32.capacity(), 32 * 1024 - META);
        assert_eq!(BytePageSize::Size48.capacity(), 48 * 1024 - META);
        assert_eq!(BytePageSize::Size64.capacity(), 64 * 1024 - META);
        assert_eq!(BytePageSize::Size128.capacity(), 128 * 1024 - META);
        assert_eq!(BytePageSize::Size256.capacity(), 256 * 1024 - META);
        assert_eq!(BytePageSize::Unset.capacity(), 64 * 1024 - META);
        assert_eq!(BytePageSize::Size4.half_capacity(), 2 * 1024);
        assert_eq!(BytePageSize::Size8.half_capacity(), 4 * 1024);
        assert_eq!(BytePageSize::Size16.half_capacity(), 8 * 1024);
        assert_eq!(BytePageSize::Size24.half_capacity(), 12 * 1024);
        assert_eq!(BytePageSize::Size32.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size48.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size64.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size128.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Size256.half_capacity(), 16 * 1024);
        assert_eq!(BytePageSize::Unset.half_capacity(), 16 * 1024);
    }

    #[test]
    fn page_size_for_capacity() {
        assert_eq!(BytePageSize::for_capacity(0), BytePageSize::Size4);
        for size in PAGE_SIZES {
            let cap = size.capacity();
            assert_eq!(BytePageSize::for_capacity(cap), size);
            if size != BytePageSize::Size4 {
                assert_eq!(BytePageSize::for_capacity(size.prev().capacity() + 1), size);
            }
        }
        assert_eq!(
            BytePageSize::for_capacity(BytePageSize::Size256.capacity() + 1),
            BytePageSize::Unset
        );
        assert_eq!(BytePageSize::for_capacity(usize::MAX), BytePageSize::Unset);
    }

    #[test]
    fn page_size_next_prev() {
        let mut size = BytePageSize::Size4;
        for expected in &PAGE_SIZES[1..] {
            size = size.next();
            assert_eq!(size, *expected);
        }
        assert_eq!(size.next(), BytePageSize::Unset);
        assert_eq!(BytePageSize::Unset.next(), BytePageSize::Unset);

        let mut size = BytePageSize::Unset;
        for expected in PAGE_SIZES.iter().rev() {
            size = size.prev();
            assert_eq!(size, *expected);
        }
        assert_eq!(size.prev(), BytePageSize::Size4);

        for pair in PAGE_SIZES.windows(2) {
            assert!(pair[0].capacity() < pair[1].capacity());
        }
    }
}
