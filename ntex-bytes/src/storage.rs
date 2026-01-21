//! # Data storage modes
//!
//! The goal of `bytes` is to be as efficient as possible for server networking
//! workloads. As such, `bytes` needs to handle buffers that are never shared,
//! shared on a single thread, and shared across many threads. `bytes` also needs
//! to handle both tiny buffers as well as very large buffers.
//!
//! To achieve high performance in these various situations, `Bytes` and
//! `BytesMut` use different strategies for storing the buffer depending on the
//! usage pattern.
//!
//! ## Shared vec buffer
//!
//! `BytesMut` always allocates and is backed by a `SharedVec`. A `SharedVec` is
//! similar to `Vec<u8>`, except that it stores the vector parameters on the heap
//! and includes a reference counter for shared buffer support. `BytesMut` owns
//! the tail of the buffer; the buffer can be modified only via `BytesMut`. The
//! head of the buffer may be owned by `Bytes`.
//!
//! ## Inlining small buffers
//!
//! The `Bytes` struct requires three pointer-sized fields. On 64-bit systems,
//! this ends up being 24 bytes, which is a significant amount of storage for
//! cases where `Bytes` is used to represent small byte strings, such as HTTP
//! header names and values.
//!
//! To avoid any allocation in these cases, `Bytes` uses the struct itself to
//! store the buffer, reserving one byte for metadata. This means that, on 64-bit
//! systems, buffers of up to 23 bytes require no allocation at all.
//!
//! The metadata byte stores a 2-bit flag indicating that the buffer is stored
//! inline, as well as 6 bits for tracking the buffer length (the return value of
//! `Bytes::len`).
//!
//! ## Static buffers
//!
//! `Bytes` can also represent a static buffer, which is created with
//! `Bytes::from_static`. No copying or allocations are required for static
//! buffers. A pointer to the `&'static [u8]`, the length, and a flag indicating
//! that the `Bytes` instance represents a static buffer are stored directly in
//! the `Bytes` struct.
//!
//! # Struct layout
//!
//! `Bytes` is a wrapper around `Storage`, which provides the data fields as well
//! as all function implementations.
//!
//! The `Storage` struct contains the following fields:
//!
//! * `ptr: *mut u8`
//! * `len: usize`
//! * `offset: usize`
//!
//! ## `ptr: *mut u8`
//!
//! A pointer to the start of the handle’s buffer view. When backed by a
//! `SharedVec`, this pointer is shifted to point somewhere inside the buffer.
//!
//! When in inline mode, `ptr` is used as part of the inlined buffer.
//!
//! ## `len: usize`
//!
//! The length of the handle’s buffer view. When backed by a `SharedVec`, this is
//! the length of the buffer slice. The slice represented by `ptr` and `len`
//! always points to initialized memory.
//!
//! When in inline mode, `len` is used as part of the inlined buffer.
//!
//! ## `offset: usize`
//!
//! The lower two bits of `offset` are used to track the storage mode of
//! `Storage`. `0b01` indicates shared storage, `0b10` indicates inline storage,
//! and `0b11` indicates static storage. The remaining upper bits store the
//! offset value.
//!
//! When storage is backed by a `SharedVec`, `offset` represents the offset of
//! the pointer and is used to calculate the pointer to the `SharedVec`.
//! `ptr - offset` always points to the beginning of the `SharedVec` structure.
//!
//! When in inline mode, `offset` is used as part of the inlined buffer.
//!
//! On little-endian platforms, the `offset` field must be the first field in the
//! struct. On big-endian platforms, the `offset` field must be the last field in
//! the struct. Since a deterministic struct layout is required, `Storage` is
//! annotated with `#[repr(C)]`.
//!
//! # Thread safety
//!
//! `Bytes::clone()` returns a new `Bytes` handle without copying. This is done by
//! incrementing the buffer’s reference count and returning a new struct pointing
//! to the same buffer.
//!
//! Care is taken to minimize the need for synchronization. Most operations do
//! not require any synchronization.
//!
use crate::alloc::alloc::{self, Layout, LayoutError};

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{self, AtomicUsize};
use std::{cmp, mem, num::NonZeroUsize, ptr, ptr::NonNull, slice};

use crate::{info::Info, info::Kind};

#[cfg(target_endian = "little")]
#[repr(C)]
pub(crate) struct Storage {
    offset: NonZeroUsize,
    ptr: *mut u8,
    len: usize,
}

#[cfg(target_endian = "big")]
#[repr(C)]
pub(crate) struct Storage {
    len: usize,
    ptr: *mut u8,
    offset: NonZeroUsize,
}

/// Thread-safe reference-counted container for the shared storage.
struct SharedVec {
    len: u32,
    offset: u32,
    remaining: u32,
    ref_count: AtomicUsize,
}

pub(crate) struct StorageVec(NonNull<SharedVec>);

// Buffer storage strategy flags.
const KIND_VEC: usize = 0b01;
const KIND_INLINE: usize = 0b10;
const KIND_STATIC: usize = 0b11;
const KIND_MASK: usize = 0b11;
// const KIND_UNMASK: usize = !KIND_MASK;
const KIND_OFFSET_BITS: usize = 2;

pub const METADATA_SIZE: usize = mem::size_of::<SharedVec>();
const METADATA_SIZE_U32: u32 = METADATA_SIZE as u32;

// Bit op constants for extracting the inline length value from the `ptr` field.
const INLINE_LEN_MASK: usize = 0b1111_1100;

// Byte offset from the start of `Storage` to where the inline buffer data
// starts. On little endian platforms, the first byte of the struct is the
// storage flag, so the data is shifted by a byte. On big endian systems, the
// data starts at the beginning of the struct.
#[cfg(target_endian = "little")]
const INLINE_DATA_OFFSET: isize = 1;
#[cfg(target_endian = "big")]
const INLINE_DATA_OFFSET: isize = 0;

// Inline buffer capacity. This is the size of `Storage` minus 1 byte for the
// metadata.
#[cfg(target_pointer_width = "64")]
pub(crate) const INLINE_CAP: usize = 3 * 8 - 1;
#[cfg(target_pointer_width = "32")]
pub(crate) const INLINE_CAP: usize = 3 * 4 - 1;

// Inline storage
const PTR_INLINE: NonZeroUsize = NonZeroUsize::new(KIND_INLINE).unwrap();
// Static storage
const PTR_STATIC: NonZeroUsize = NonZeroUsize::new(KIND_STATIC).unwrap();
// Default offset
const DEFAUILT_OFFSET: NonZeroUsize =
    NonZeroUsize::new((METADATA_SIZE << KIND_OFFSET_BITS) ^ KIND_VEC).unwrap();

/*
 *
 * ===== Storage =====
 *
 */

impl Storage {
    #[inline]
    pub(crate) const fn empty() -> Storage {
        Storage {
            ptr: ptr::null_mut(),
            len: 0,
            offset: PTR_INLINE,
        }
    }

    #[inline]
    pub(crate) const fn from_static(bytes: &'static [u8]) -> Storage {
        let ptr = bytes.as_ptr() as *mut _;

        Storage {
            ptr,
            len: bytes.len(),
            offset: PTR_STATIC,
        }
    }

    #[inline]
    pub(crate) fn from_slice(src: &[u8]) -> Storage {
        if src.len() <= INLINE_CAP {
            unsafe { Storage::from_ptr_inline(src.as_ptr(), src.len()) }
        } else {
            Storage::from_slice_with_capacity(src.len(), src)
        }
    }

    #[inline]
    fn from_slice_with_capacity(cap: usize, src: &[u8]) -> Storage {
        unsafe {
            let shared = SharedVec::create(cap, src);
            Storage {
                len: src.len(),
                ptr: shared.as_ptr().add(1) as *mut u8,
                offset: DEFAUILT_OFFSET,
            }
        }
    }

    #[inline]
    unsafe fn from_ptr_inline(src: *const u8, len: usize) -> Storage {
        let mut st = Storage {
            ptr: ptr::null_mut(),
            len: 0,
            offset: PTR_INLINE,
        };

        let dst = st.inline_ptr();
        ptr::copy(src, dst, len);
        st.set_inline_len(len);
        st
    }

    /// Return a slice for the handle's view into the shared buffer
    #[inline]
    pub(crate) fn as_ref(&self) -> &[u8] {
        unsafe {
            if self.kind() == KIND_INLINE {
                slice::from_raw_parts(self.inline_ptr_ro(), self.inline_len())
            } else {
                slice::from_raw_parts(self.ptr, self.len)
            }
        }
    }

    /// Return a raw pointer to data
    #[inline]
    unsafe fn as_ptr(&mut self) -> *mut u8 {
        unsafe {
            if self.kind() == KIND_INLINE {
                self.inline_ptr()
            } else {
                self.ptr
            }
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        if self.kind() == KIND_INLINE {
            self.inline_len()
        } else {
            self.len
        }
    }

    /// Pointer to the start of the inline buffer
    #[inline]
    unsafe fn inline_ptr(&mut self) -> *mut u8 {
        (self as *mut Storage as *mut u8).offset(INLINE_DATA_OFFSET)
    }

    /// Pointer to the start of the inline buffer
    #[inline]
    unsafe fn inline_ptr_ro(&self) -> *const u8 {
        (self as *const Storage as *const u8).offset(INLINE_DATA_OFFSET)
    }

    #[inline]
    fn inline_len(&self) -> usize {
        (self.offset.get() & INLINE_LEN_MASK) >> KIND_OFFSET_BITS
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        let kind = self.kind();
        match kind {
            KIND_VEC => unsafe { (*self.shared_vec()).capacity() },
            KIND_INLINE => INLINE_CAP,
            _ => self.len,
        }
    }

    pub(crate) fn split_off(&mut self, at: usize, create_inline: bool) -> Storage {
        let other = unsafe {
            if create_inline && self.len() - at <= INLINE_CAP {
                Storage::from_ptr_inline(self.as_ptr().add(at), self.len() - at)
            } else {
                let mut other = self.shallow_clone();
                other.set_start(at);
                other
            }
        };
        unsafe {
            if create_inline && at <= INLINE_CAP {
                *self = Storage::from_ptr_inline(self.as_ptr(), at);
            } else {
                self.set_end(at);
            }
        }

        other
    }

    pub(crate) fn split_to(&mut self, at: usize) -> Storage {
        let other = unsafe {
            if at <= INLINE_CAP {
                Storage::from_ptr_inline(self.as_ptr(), at)
            } else {
                let mut other = self.shallow_clone();
                other.set_end(at);
                other
            }
        };
        unsafe {
            self.set_start(at);
        }

        other
    }

    pub(crate) fn truncate(&mut self, len: usize, create_inline: bool) {
        unsafe {
            if len <= self.len() {
                if create_inline && len < INLINE_CAP {
                    *self = Storage::from_ptr_inline(self.as_ptr(), len);
                } else {
                    self.set_len(len);
                }
            }
        }
    }

    pub(crate) fn trimdown(&mut self) {
        let kind = self.kind();

        // trim down only if buffer is not inline or static and
        // buffer's unused space is greater than 64 bytes
        if !(kind == KIND_INLINE || kind == KIND_STATIC) {
            if self.len() <= INLINE_CAP {
                *self = unsafe { Storage::from_ptr_inline(self.as_ptr(), self.len()) };
            } else if self.capacity() - self.len() >= 64 {
                *self = Storage::from_slice_with_capacity(self.len(), self.as_ref());
            }
        }
    }

    #[inline]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        let kind = self.kind();
        match kind {
            KIND_VEC => {
                assert!(len <= self.capacity());
                self.len = len;
            }
            KIND_INLINE => self.set_inline_len(len),
            _ => {
                assert!(len <= self.len);
                self.len = len;
            }
        }
    }

    /// Set the length of the inline buffer. This is done by writing to the
    /// least significant byte of the `arc` field.
    #[inline]
    fn set_inline_len(&mut self, len: usize) {
        debug_assert!(len <= INLINE_CAP);
        self.offset = unsafe {
            NonZeroUsize::new_unchecked(
                self.offset.get() & !INLINE_LEN_MASK | (len << KIND_OFFSET_BITS),
            )
        };
    }

    pub(crate) unsafe fn set_start(&mut self, start: usize) {
        // Setting the start to 0 is a no-op, so return early if this is the
        // case.
        if start == 0 {
            return;
        }

        match self.kind() {
            KIND_VEC => {
                let shared = self.shared_vec();

                // Updating the start of the view is setting `ptr` to point to the
                // new start and updating the `len` field to reflect the new length
                // of the view.
                let offset = (self.offset.get() >> KIND_OFFSET_BITS) + start;

                self.ptr = (shared as *mut u8).add(offset);
                if self.len >= start {
                    self.len -= start;
                } else {
                    self.len = 0;
                }

                self.offset =
                    NonZeroUsize::new_unchecked((offset << KIND_OFFSET_BITS) ^ KIND_VEC);
            }
            KIND_INLINE => {
                assert!(start <= INLINE_CAP);

                let len = self.inline_len();
                if len <= start {
                    self.set_inline_len(0);
                } else {
                    // `set_start` is essentially shifting data off the front of the
                    // view. Inlined buffers only track the length of the slice.
                    // So, to update the start, the data at the new starting point
                    // is copied to the beginning of the buffer.
                    let new_len = len - start;

                    let dst = self.inline_ptr();
                    let src = (dst as *const u8).add(start);

                    ptr::copy(src, dst, new_len);

                    self.set_inline_len(new_len);
                }
            }
            _ => {
                // set len for static storage
                assert!(start <= self.len);
                self.len -= start;
                self.ptr = self.ptr.add(start);
            }
        }
    }

    pub(crate) unsafe fn set_end(&mut self, end: usize) {
        match self.kind() {
            KIND_VEC => {
                self.len = cmp::min(self.len, end);
            }
            KIND_INLINE => {
                assert!(end <= INLINE_CAP);
                let new_len = cmp::min(self.inline_len(), end);
                self.set_inline_len(new_len);
            }
            _ => {
                // set len for static storage
                assert!(end <= self.len);
                self.len = end;
            }
        }
    }

    #[inline]
    /// Increments the ref count. This should only be done if it is known that
    /// it can be done safely. As such, this fn is not public, instead other
    /// fns will use this one while maintaining the guarantees.
    /// Parameter `mut_self` should only be set to `true` if caller holds
    /// `&mut self` reference.
    ///
    /// "Safely" is defined as not exposing two `BytesMut` values that point to
    /// the same byte window.
    ///
    /// This function is thread safe.
    unsafe fn shallow_clone(&self) -> Storage {
        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Storage` struct fields will be gibberish.
        //
        // Additionally, if kind is STATIC, then ptr is *never* changed, making
        // it safe and faster to check for it now before an atomic acquire.
        //
        // The value returned by `kind` isn't itself safe, but the value could
        // inform what operations to take, and unsafely do something without
        // synchronization.
        //
        // KIND_INLINE and KIND_STATIC will *never* change, so branches on that
        // information is safe.
        let kind = self.kind();

        if kind == KIND_INLINE || kind == KIND_STATIC {
            // In this case, a shallow_clone still involves copying the data.
            let mut inner: mem::MaybeUninit<Storage> = mem::MaybeUninit::uninit();
            ptr::copy_nonoverlapping(self, inner.as_mut_ptr(), 1);
            inner.assume_init()
        } else {
            // ptr points to SharedVec
            let shared = self.shared_vec();
            let ref_cnt = (*shared).ref_count.fetch_add(1, Relaxed);
            if ref_cnt == usize::MAX {
                abort();
            }

            Storage { ..*self }
        }
    }

    /// Returns true if the buffer is stored inline
    #[inline]
    pub(crate) fn is_inline(&self) -> bool {
        self.kind() == KIND_INLINE
    }

    #[inline]
    fn shared_vec(&self) -> *mut SharedVec {
        let offset = self.offset.get() >> KIND_OFFSET_BITS;
        unsafe { self.ptr.sub(offset) as *mut SharedVec }
    }

    #[inline]
    fn kind(&self) -> usize {
        // This function is going to probably raise some eyebrows. The function
        // returns true if the buffer is stored inline. This is done by checking
        // the least significant bit in the `ptr` field.
        //
        // Now, you may notice that `ptr` is an `AtomicPtr` and this is
        // accessing it as a normal field without performing an atomic load...
        //
        // Again, the function only cares about the least significant bit, and
        // this bit is set when `Storage` is created and never changed after that.
        // All platforms have atomic "word" operations and won't randomly flip
        // bits, so even without any explicit atomic operations, reading the
        // flag will be correct.
        //
        // This function is very critical performance wise as it is called for
        // every operation. Performing an atomic load would mess with the
        // compiler's ability to optimize. Simple benchmarks show up to a 10%
        // slowdown using a `Relaxed` atomic load on x86.

        #[cfg(target_endian = "little")]
        #[inline]
        fn imp(ptr: usize) -> usize {
            ptr & KIND_MASK
        }

        #[cfg(target_endian = "big")]
        #[inline]
        fn imp(arc: usize) -> usize {
            unsafe {
                let p: *const u8 = arc as *const u8;
                *p & KIND_MASK
            }
        }

        imp(self.offset.get())
    }

    pub(crate) fn info(&self) -> Info {
        let kind = self.kind();

        let (id, refs, capacity) = unsafe {
            if kind == KIND_VEC {
                let ptr = self.shared_vec();
                (
                    ptr as usize,
                    (*ptr).ref_count.load(Relaxed),
                    (*ptr).offset as usize
                        + (*ptr).len as usize
                        + (*ptr).remaining as usize,
                )
            } else {
                (0, 0, 0)
            }
        };

        Info {
            id,
            refs,
            capacity,
            kind: Kind::from_raw(kind),
        }
    }
}

unsafe impl Send for Storage {}
unsafe impl Sync for Storage {}

impl Clone for Storage {
    fn clone(&self) -> Storage {
        unsafe { self.shallow_clone() }
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        if self.kind() == KIND_VEC {
            release_shared_vec(self.shared_vec());
        }
    }
}

impl StorageVec {
    /// Create new empty storage with specified capacity
    pub(crate) fn with_capacity(capacity: usize) -> StorageVec {
        StorageVec(SharedVec::create(capacity, &[]))
    }

    /// Create new storage with capacity and copy slice
    ///
    /// Caller must garantee cap is larger or eaqual to src length
    pub(crate) fn from_slice(capacity: usize, src: &[u8]) -> StorageVec {
        StorageVec(SharedVec::create(capacity, src))
    }

    /// Return a slice for the handle's view into the shared buffer
    pub(crate) fn as_ref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.as_ptr(), self.len()) }
    }

    /// Return a mutable slice for the handle's view into the shared buffer
    pub(crate) fn as_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.as_ptr(), self.len()) }
    }

    /// Return a mutable slice for the handle's view into the shared buffer
    /// including potentially uninitialized bytes.
    pub(crate) unsafe fn as_raw(&mut self) -> &mut [u8] {
        slice::from_raw_parts_mut(self.as_ptr(), self.capacity())
    }

    /// Return a raw pointer to data
    pub(crate) unsafe fn as_ptr(&self) -> *mut u8 {
        (self.0.as_ptr() as *mut u8).add((*self.0.as_ptr()).offset as usize)
    }

    unsafe fn as_inner(&mut self) -> &mut SharedVec {
        self.0.as_mut()
    }

    /// Insert a byte into the next slot and advance the len by 1.
    pub(crate) fn put_u8(&mut self, n: u8) {
        let len = self.len();
        unsafe {
            let inner = self.as_inner();
            inner.len += 1;
            inner.remaining -= 1;
            *self.as_ptr().add(len) = n;
        }
    }

    pub(crate) fn len(&self) -> usize {
        unsafe { (*self.0.as_ptr()).len as usize }
    }

    pub(crate) fn capacity(&self) -> usize {
        let ptr = unsafe { &*self.0.as_ptr() };
        ptr.len as usize + ptr.remaining as usize
    }

    pub(crate) fn remaining(&self) -> usize {
        unsafe { (*self.0.as_ptr()).remaining as usize }
    }

    pub(crate) fn freeze(self) -> Storage {
        unsafe {
            if self.len() <= INLINE_CAP {
                Storage::from_ptr_inline(self.as_ptr(), self.len())
            } else {
                let inner = self.0.as_ref();
                let offset = inner.offset as usize;

                let inner = Storage {
                    ptr: (self.0.as_ptr() as *mut u8).add(offset),
                    len: self.len(),
                    offset: NonZeroUsize::new_unchecked(
                        (offset << KIND_OFFSET_BITS) ^ KIND_VEC,
                    ),
                };
                mem::forget(self);
                inner
            }
        }
    }

    pub(crate) fn split_to(&mut self, at: usize) -> Storage {
        unsafe {
            let ptr = self.as_ptr();

            let other = if at <= INLINE_CAP {
                Storage::from_ptr_inline(ptr, at)
            } else {
                let inner = self.as_inner();
                inner.ref_count.fetch_add(1, Relaxed);

                let offset = inner.offset as usize;
                Storage {
                    ptr: (self.0.as_ptr() as *mut u8).add(offset),
                    len: at,
                    offset: NonZeroUsize::new_unchecked(
                        (offset << KIND_OFFSET_BITS) ^ KIND_VEC,
                    ),
                }
            };
            self.set_start(at as u32);

            other
        }
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        unsafe {
            // try to reclaim the buffer. This is possible if the current
            // handle is the only outstanding handle pointing to the buffer.
            if len == 0 {
                let inner = self.as_inner();
                if inner.is_unique() && inner.offset != METADATA_SIZE_U32 {
                    let cap = (inner.offset as usize)
                        + inner.len as usize
                        + inner.remaining as usize;
                    inner.len = 0;
                    inner.offset = METADATA_SIZE_U32;
                    inner.remaining = (cap - METADATA_SIZE) as u32;
                    return;
                }
            }

            if len < self.len() {
                self.set_len(len);
            }
        }
    }

    pub(crate) fn resize(&mut self, new_len: usize, value: u8) {
        let len = self.len();
        if new_len > len {
            let additional = new_len - len;
            self.reserve(additional);
            unsafe {
                let dst = self.as_raw()[len..].as_mut_ptr();
                ptr::write_bytes(dst, value, additional);
                self.set_len(new_len);
            }
        } else {
            self.truncate(new_len);
        }
    }

    /// Copy data for new storage
    #[inline]
    pub(crate) fn reserve_capacity(&mut self, capacity: usize) {
        *self = StorageVec(SharedVec::create(capacity, self.as_ref()));
    }

    #[inline]
    pub(crate) fn reserve(&mut self, additional: usize) {
        if additional <= self.remaining() {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.reserve_inner(additional)
    }

    fn reserve_inner(&mut self, additional: usize) {
        unsafe {
            let inner = self.as_inner();
            let len = inner.len as usize;

            // Reserving involves abandoning the currently shared buffer and
            // allocating a new vector with the requested capacity.
            let new_cap = len + additional;

            if inner.is_unique() {
                let capacity = (inner.offset as usize)
                    + (inner.len as usize)
                    + (inner.remaining as usize);

                // try to reclaim the buffer. This is possible if the current
                // handle is the only outstanding handle pointing to the buffer.
                if capacity >= (new_cap + METADATA_SIZE) {
                    let offset = inner.offset;
                    inner.offset = METADATA_SIZE_U32;
                    inner.remaining = (capacity - len - METADATA_SIZE) as u32;

                    // The capacity is sufficient, reclaim the buffer
                    if len != 0 {
                        let ptr = self.0.as_ptr() as *mut u8;
                        ptr::copy(ptr.add(offset as usize), ptr.add(METADATA_SIZE), len);
                    }
                    return;
                }
            }
            // Create a new storage
            *self = StorageVec(SharedVec::create(new_cap, self.as_ref()));
        }
    }

    #[inline]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        let cap = self.capacity();
        assert!(len <= cap);

        let vec = self.0.as_mut();
        vec.len = len as u32;
        vec.remaining = (cap - len) as u32;
    }

    pub(crate) unsafe fn set_start(&mut self, start: u32) {
        if start != 0 {
            let cap = self.capacity();
            let inner = self.as_inner();

            assert!(
                start <= cap as u32,
                "Cannot set start position offset:{} len:{} remaining:{}, new-len:{start}",
                inner.offset,
                inner.len,
                inner.remaining,
            );

            // Updating the start of the view is setting `offset` to point to the
            // new start and updating the `len` field to reflect the new length
            // of the view.
            inner.offset += start;

            if inner.len > start {
                inner.len -= start;
            } else {
                inner.len = 0;
            }
            inner.remaining = cap as u32 - inner.len - start;
        }
    }
}

unsafe impl Send for StorageVec {}
unsafe impl Sync for StorageVec {}

impl Drop for StorageVec {
    fn drop(&mut self) {
        release_shared_vec(self.0.as_ptr());
    }
}

impl SharedVec {
    fn create(cap: usize, src: &[u8]) -> NonNull<SharedVec> {
        let ptr = Self::alloc_with_capacity(cap, src.len() as u32);

        // copy slice
        unsafe {
            let dst = ptr.add(METADATA_SIZE);
            let sl = slice::from_raw_parts_mut(dst, src.len());
            sl.copy_from_slice(src);
            NonNull::new_unchecked(ptr as *mut SharedVec)
        }
    }

    fn alloc_with_capacity(cap: usize, len: u32) -> *mut u8 {
        let layout = shared_vec_layout(cap).unwrap();

        // Alloc memory and store data
        unsafe {
            let ptr = alloc::alloc(layout);
            if ptr.is_null() {
                alloc::handle_alloc_error(layout);
            }

            ptr::write(
                ptr as *mut SharedVec,
                SharedVec {
                    len,
                    offset: METADATA_SIZE_U32,
                    remaining: (layout.size() - METADATA_SIZE - len as usize) as u32,
                    ref_count: AtomicUsize::new(1),
                },
            );
            ptr
        }
    }

    fn is_unique(&self) -> bool {
        // This is same as Shared::is_unique() but for KIND_VEC
        self.ref_count.load(Acquire) == 1
    }

    fn capacity(&self) -> usize {
        self.len as usize + self.remaining as usize
    }
}

fn release_shared_vec(ptr: *mut SharedVec) {
    // `Shared` storage... follow the drop steps from Arc.
    unsafe {
        if (*ptr).ref_count.fetch_sub(1, Release) != 1 {
            return;
        }

        // This fence is needed to prevent reordering of use of the data and
        // deletion of the data.  Because it is marked `Release`, the decreasing
        // of the reference count synchronizes with this `Acquire` fence. This
        // means that use of the data happens before decreasing the reference
        // count, which happens before this fence, which happens before the
        // deletion of the data.
        //
        // As explained in the [Boost documentation][1],
        //
        // > It is important to enforce any possible access to the object in one
        // > thread (through an existing reference) to *happen before* deleting
        // > the object in a different thread. This is achieved by a "release"
        // > operation after dropping a reference (any access to the object
        // > through this reference must obviously happened before), and an
        // > "acquire" operation before deleting the object.
        //
        // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
        atomic::fence(Acquire);

        // Drop the data
        let cap = (*ptr).offset as usize + (*ptr).remaining as usize + (*ptr).len as usize;
        ptr::drop_in_place(ptr);
        let layout = shared_vec_layout(cap - METADATA_SIZE).unwrap();
        alloc::dealloc(ptr as *mut _, layout);
    }
}

const fn shared_vec_layout(cap: usize) -> Result<Layout, LayoutError> {
    let s_layout = match Layout::from_size_align(cap, Layout::new::<u8>().align()) {
        Ok(l) => l,
        Err(e) => return Err(e),
    };
    match Layout::new::<SharedVec>().pad_to_align().extend(s_layout) {
        Ok((l, _)) => Ok(l),
        Err(err) => Err(err),
    }
}

// While there is `std::process:abort`, it's only available in Rust 1.17, and
// our minimum supported version is currently 1.15. So, this acts as an abort
// by triggering a double panic, which always aborts in Rust.
struct Abort;

impl Drop for Abort {
    fn drop(&mut self) {
        panic!();
    }
}

impl Kind {
    fn from_raw(n: usize) -> Kind {
        match n {
            KIND_VEC => Kind::Vec,
            KIND_INLINE => Kind::Inline,
            KIND_STATIC => Kind::Static,
            _ => Kind::Vec,
        }
    }
}

#[inline(never)]
#[cold]
fn abort() {
    let _a = Abort;
    panic!();
}

#[cfg(test)]
mod tests {
    use crate::*;

    const LONG: &[u8] =
        b"mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb";

    #[test]
    fn trimdown() {
        let mut b = Bytes::from(LONG.to_vec());
        assert_eq!(b.storage.capacity(), 263);
        unsafe { b.storage.set_len(68) };
        assert_eq!(b.len(), 68);
        assert_eq!(&b[..], &LONG[..68]);
        assert_eq!(b.storage.capacity(), 263);
        b.trimdown();
        assert_eq!(&b[..], &LONG[..68]);
        assert_eq!(b.storage.capacity(), 68);

        unsafe { b.storage.set_len(16) };
        assert_eq!(&b[..], &LONG[..16]);
        b.trimdown();
        assert!(b.is_inline());
    }

    #[test]
    #[allow(clippy::unnecessary_fallible_conversions)]
    fn bytes_mut() {
        let bv = BytesMut::copy_from_slice(LONG);
        assert_eq!(bv.capacity(), 263);
        assert_eq!(bv.len(), 263);
        assert_eq!(bv.as_ref().len(), 263);
        assert_eq!(bv.as_ref(), LONG);
        assert_eq!(&bv[..], LONG);

        let sl: &[u8] = &[];
        let bv = BytesMut::copy_from_slice(sl);
        assert_eq!(bv.capacity(), 0);
        assert_eq!(bv.len(), 0);
        assert_eq!(bv.as_ref().len(), 0);
        assert_eq!(bv.as_ref(), sl);
        assert_eq!(&bv[..], sl);

        let mut bv = BytesMut::copy_from_slice(&b"hello"[..]);
        assert_eq!(bv.capacity(), 5);
        bv.reserve_capacity(128);
        assert_eq!(bv.capacity(), 128);
        assert_eq!(bv.len(), 5);
        assert_eq!(bv.as_ref(), &b"hello"[..]);

        let mut bv = BytesMut::copy_from_slice(&b"hello"[..]);
        assert_eq!(bv.capacity(), 5);
        assert_eq!(bv.len(), 5);
        assert_eq!(bv.as_ref().len(), 5);
        assert_eq!(bv.as_ref()[0], b"h"[0]);
        assert_eq!(bv.remaining_mut(), 0);
        bv.reserve(1);
        assert_eq!(bv.remaining_mut(), 1);
        bv.put_u8(b" "[0]);
        assert_eq!(bv.as_ref(), &b"hello "[..]);
        assert_eq!(bv.remaining_mut(), 0);
        bv.reserve(5);
        assert_eq!(bv.remaining_mut(), 5);
        bv.put("world");
        assert_eq!(bv, "hello world");
        bv.advance_to(6);
        assert_eq!(bv, "world");
        assert_eq!(bv.remaining_mut(), 0);

        let bv = BytesMut::copy_from_slice(&b"hello world"[..]);
        let b = Bytes::from(bv);
        assert_eq!(b, "hello world");

        // does not re-alloc
        let mut bv = BytesMut::with_capacity(0);
        bv.extend_from_slice(b"hello world.");
        bv.extend_from_slice(b"hello world.");
        bv.extend_from_slice(b"hello world.");
        bv.extend_from_slice(b"hello world.");
        let p1 = unsafe { bv.storage.as_ptr() as usize };

        bv.advance(48);
        assert!(bv.is_empty());
        assert_eq!(bv.capacity(), 0);
        bv.reserve(48);
        assert!(bv.is_empty());
        assert_eq!(bv.capacity(), 48);
        let p2 = unsafe { bv.storage.as_ptr() as usize };
        assert!(p1 == p2);
    }
}
