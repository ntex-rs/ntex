use crate::alloc::alloc::{self, Layout, LayoutError};

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{self, AtomicU32};
use std::{cell::Cell, cmp, mem, num::NonZeroUsize, ptr, ptr::NonNull, slice};

use crate::storage::{INLINE_CAP, MIN_CAPACITY, Storage};
use crate::{BytePageSize, buf::UninitSlice};

#[derive(Debug)]
/// Thread-safe reference-counted container for the shared storage.
pub(crate) struct SharedVec {
    /// Start of the `BytesMut` view, from the beginning of the allocation.
    pub(crate) offset: u32,
    /// Length of the `BytesMut` view.
    pub(crate) len: u32,
    /// Data capacity of the whole allocation, excluding the header.
    ///
    /// It is not modified while the buffer is shared, so `Bytes` handles
    /// can read it concurrently with the `BytesMut` handle modifying
    /// the other fields.
    ///
    /// The spare capacity of the view (`capacity + METADATA_SIZE - offset - len`)
    /// and the page size class (by allocation size) are derived from it.
    pub(crate) capacity: u32,
    pub(crate) ref_count: AtomicU32,
}

#[derive(Debug)]
pub(crate) struct StorageVec(pub(crate) NonNull<SharedVec>);

// Buffer storage strategy flags.
const KIND_VEC: usize = 0b01;
const KIND_OFFSET_BITS: usize = 2;

/// Size of the header stored in front of the data of every heap buffer.
pub const METADATA_SIZE: usize = mem::size_of::<SharedVec>();
const METADATA_SIZE_U32: u32 = METADATA_SIZE as u32;

/// Maximum buffer capacity, offsets and sizes are stored as `u32`.
pub(crate) const MAX_CAPACITY: usize = u32::MAX as usize - METADATA_SIZE;

/// Largest capacity step of `reserve_more` for buffers without a page size.
const MAX_MORE_STEP: usize = 64 * 1024;

impl StorageVec {
    /// Create new empty storage with specified capacity
    pub(crate) fn with_capacity(capacity: usize) -> StorageVec {
        StorageVec(SharedVec::create(capacity, &[]))
    }

    /// Create new empty storage with specified size category
    pub(crate) fn sized(size: BytePageSize) -> StorageVec {
        // the cache is unavailable while the thread-local is being destroyed
        let cached = CACHE
            .try_with(|c| {
                let mut cst = c.take()?;
                let item = cst.cache[size as usize].pop();
                c.set(Some(cst));
                item
            })
            .ok()
            .flatten();

        if let Some(item) = cached {
            item
        } else {
            StorageVec(SharedVec::create(size.capacity(), &[]))
        }
    }

    /// Create new storage with capacity and copy slice
    ///
    /// Panics if `capacity` is smaller than `src` length
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
        (self.0.as_ptr().cast::<u8>()).add((*self.0.as_ptr()).offset as usize)
    }

    /// Returns a raw pointer to the header.
    ///
    /// A `&mut SharedVec` must not be created, other handles access
    /// `ref_count` and `capacity` concurrently.
    fn as_inner(&mut self) -> *mut SharedVec {
        self.0.as_ptr()
    }

    /// Insert a byte into the next slot and advance the len by 1.
    pub(crate) fn put_u8(&mut self, n: u8) {
        let len = self.len();
        unsafe {
            let inner = self.as_inner();
            (*inner).len += 1;
            *self.as_ptr().add(len) = n;
        }
    }

    /// Returns the spare capacity after the data.
    #[inline]
    pub(crate) fn spare_mut(&mut self) -> &mut UninitSlice {
        // SAFETY: `remaining` bytes after `len` are allocated and owned by this view
        unsafe { UninitSlice::from_raw_parts_mut(self.as_ptr().add(self.len()), self.remaining()) }
    }

    /// Appends as much of `src` as fits, returns the number of bytes copied.
    #[inline]
    pub(crate) fn put_slice_partial(&mut self, src: &[u8]) -> usize {
        let cnt = cmp::min(src.len(), self.remaining());
        self.spare_mut()[..cnt].copy_from_slice(&src[..cnt]);
        unsafe { self.set_len(self.len() + cnt) };
        cnt
    }

    pub(crate) fn len(&self) -> usize {
        unsafe { (*self.0.as_ptr()).len as usize }
    }

    pub(crate) fn capacity(&self) -> usize {
        unsafe {
            let inner = self.0.as_ref();
            (inner.capacity + METADATA_SIZE_U32 - inner.offset) as usize
        }
    }

    #[inline]
    pub(crate) fn remaining(&self) -> usize {
        unsafe {
            let inner = self.0.as_ref();
            (inner.capacity + METADATA_SIZE_U32 - inner.offset - inner.len) as usize
        }
    }

    pub(crate) fn is_full(&self) -> bool {
        self.remaining() == 0
    }

    pub(crate) fn is_unique(&self) -> bool {
        unsafe { (*self.0.as_ptr()).is_unique() }
    }

    /// Takes ownership of the allocation of a frozen view, if the view holds
    /// the only reference to it.
    ///
    /// The view becomes the `BytesMut` view, the rest of the allocation past
    /// it is spare capacity.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `SharedVec` referenced by the caller, the
    /// view `offset..offset + len` must be within the allocation. On success
    /// the caller's reference is transferred to the returned handle.
    pub(crate) unsafe fn from_unique_view(
        ptr: *mut SharedVec,
        offset: usize,
        len: usize,
    ) -> Option<StorageVec> {
        // `Acquire` synchronizes with the `Release` decrement of the handles
        // dropped by other threads, their accesses happen before the header
        // is updated below.
        if !(*ptr).is_unique() {
            return None;
        }
        (*ptr).offset = offset as u32;
        (*ptr).len = len as u32;
        Some(StorageVec(NonNull::new_unchecked(ptr)))
    }

    /// Returns an immutable view of the data, `self` stays usable.
    ///
    /// The view shares the allocation, so `self` is no longer unique while
    /// it exists. There is never more than one `StorageVec` per allocation.
    pub(crate) fn shallow_freeze(&self) -> Storage {
        unsafe {
            if self.len() <= INLINE_CAP {
                Storage::from_ptr_inline(self.as_ptr(), self.len())
            } else {
                let inner = self.0.as_ref();
                let ref_cnt = inner.ref_count.fetch_add(1, Relaxed);
                if ref_cnt == u32::MAX {
                    abort();
                }

                let offset = inner.offset as usize;
                Storage {
                    ptr: (self.0.as_ptr().cast::<u8>()).add(offset),
                    len: self.len(),
                    offset: NonZeroUsize::new_unchecked((offset << KIND_OFFSET_BITS) ^ KIND_VEC),
                }
            }
        }
    }

    pub(crate) fn freeze(self) -> Storage {
        unsafe {
            if self.len() <= INLINE_CAP {
                Storage::from_ptr_inline(self.as_ptr(), self.len())
            } else {
                let inner = self.0.as_ref();
                let offset = inner.offset as usize;

                let inner = Storage {
                    ptr: (self.0.as_ptr().cast::<u8>()).add(offset),
                    len: self.len(),
                    offset: NonZeroUsize::new_unchecked((offset << KIND_OFFSET_BITS) ^ KIND_VEC),
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
                let ref_cnt = (*inner).ref_count.fetch_add(1, Relaxed);
                if ref_cnt == u32::MAX {
                    abort();
                }

                let offset = (*inner).offset as usize;
                Storage {
                    ptr: (self.0.as_ptr().cast::<u8>()).add(offset),
                    len: at,
                    offset: NonZeroUsize::new_unchecked((offset << KIND_OFFSET_BITS) ^ KIND_VEC),
                }
            };
            self.set_start(at);

            other
        }
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        unsafe {
            // try to reclaim the buffer. This is possible if the current
            // handle is the only outstanding handle pointing to the buffer.
            if len == 0 {
                let inner = self.as_inner();
                if (*inner).is_unique() && (*inner).offset != METADATA_SIZE_U32 {
                    (*inner).len = 0;
                    (*inner).offset = METADATA_SIZE_U32;
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

    #[inline]
    pub(crate) fn reserve(&mut self, additional: usize) {
        if additional <= self.remaining() {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.reserve_inner(additional, false);
    }

    #[inline]
    pub(crate) fn reserve_exact(&mut self, additional: usize) {
        if additional <= self.remaining() {
            return;
        }

        self.reserve_inner(additional, true);
    }

    /// Grows the buffer by one step, see `BytesMut::reserve_more`.
    pub(crate) fn reserve_more(&mut self) {
        let size = self.page_size();
        if self.remaining() >= size.low() {
            return;
        }

        // the allocation holds the data and half a page, the unique buffer is
        // reclaimed, a shared page moves to a page of the same size
        let len = self.len();
        let half = size.half_capacity();
        if len + half <= unsafe { SharedVec::capacity(self.0.as_ptr()) } {
            if self.is_unique() {
                self.reserve_inner(half, true);
                return;
            }
            if size != BytePageSize::Unset {
                self.move_to_page(size);
                return;
            }
        }

        let next = size.next();
        if next == BytePageSize::Unset {
            let cap = self.capacity();
            let new_cap = cap.saturating_add(cap.clamp(MIN_CAPACITY, MAX_MORE_STEP));
            self.reserve_inner(new_cap - len, true);
        } else {
            // the next page comes from the page cache
            self.move_to_page(next);
        }
    }

    /// Copies the data into a page of `size`, the old buffer is released.
    fn move_to_page(&mut self, size: BytePageSize) {
        let len = self.len();
        let mut st = StorageVec::sized(size);
        unsafe {
            ptr::copy_nonoverlapping(self.as_ptr(), st.as_ptr(), len);
            st.set_len(len);
        }
        *self = st;
    }

    fn reserve_inner(&mut self, additional: usize, exact: bool) {
        unsafe {
            let inner = self.as_inner();
            let len = (*inner).len as usize;

            // A unique buffer is reclaimed or grown in place, otherwise the
            // data is copied into a new allocation.
            let new_cap = len
                .checked_add(additional)
                .expect("buffer capacity overflow");
            let grow_cap = if exact {
                new_cap
            } else {
                grown_capacity(len, new_cap)
            };
            let size = self.page_size();

            if (*inner).is_unique() {
                let capacity = (*inner).capacity as usize;

                // try to reclaim the buffer. This is possible if the current
                // handle is the only outstanding handle pointing to the buffer.
                if capacity >= new_cap {
                    let offset = (*inner).offset;
                    (*inner).offset = METADATA_SIZE_U32;

                    // The capacity is sufficient, reclaim the buffer
                    if len != 0 {
                        let ptr = self.0.as_ptr().cast::<u8>();
                        ptr::copy(ptr.add(offset as usize), ptr.add(METADATA_SIZE), len);
                    }
                    return;
                }

                // Grow the allocation instead of copying into a new one, the
                // allocator can often extend it in place. An exact reservation
                // grows a pooled page too, it is not rounded up to a page.
                if exact || size == BytePageSize::Unset {
                    self.realloc(len, capacity, grow_cap);
                    return;
                }
            }

            // A pooled page grows into a page of the category that fits the
            // new capacity, at least its own category. The old page goes back
            // to the page cache on release. Above the largest category, for
            // buffers without a page size and for exact reservations, a new
            // buffer of the exact capacity is allocated.
            if !exact && size != BytePageSize::Unset {
                let new_size = BytePageSize::for_capacity(grow_cap);
                if new_size != BytePageSize::Unset {
                    let new_size = if (new_size as usize) < (size as usize) {
                        size
                    } else {
                        new_size
                    };
                    self.move_to_page(new_size);
                    return;
                }
            }

            *self = StorageVec(SharedVec::create(grow_cap, self.as_ref()));
        }
    }

    /// Returns the page category of the buffer.
    ///
    /// It is derived from the allocation size, any allocation of a page
    /// size belongs to the page category.
    pub(crate) fn page_size(&self) -> BytePageSize {
        unsafe {
            BytePageSize::from_alloc_size(SharedVec::capacity(self.0.as_ptr()) + METADATA_SIZE)
        }
    }

    /// Grows the unique allocation to hold `new_cap` bytes.
    ///
    /// # Safety
    ///
    /// The handle must be the only reference to the allocation, `len` and
    /// `capacity` must be its current length and capacity.
    unsafe fn realloc(&mut self, len: usize, capacity: usize, new_cap: usize) {
        assert!(
            new_cap <= MAX_CAPACITY,
            "buffer capacity {new_cap} exceeds maximum {MAX_CAPACITY}"
        );
        let old_layout = shared_vec_layout(capacity).unwrap();
        let new_layout = shared_vec_layout(new_cap).unwrap();

        unsafe {
            let ptr = self.0.as_ptr();

            // move the data to the start, it is at the start of the new
            // capacity as well. The header is consistent before allocating,
            // the allocation error handler may unwind.
            let offset = (*ptr).offset as usize;
            if offset != METADATA_SIZE {
                if len != 0 {
                    let data = ptr.cast::<u8>();
                    ptr::copy(data.add(offset), data.add(METADATA_SIZE), len);
                }
                (*ptr).offset = METADATA_SIZE_U32;
            }

            let new_ptr = alloc::realloc(ptr.cast(), old_layout, new_layout.size());
            if new_ptr.is_null() {
                alloc::handle_alloc_error(new_layout);
            }

            #[allow(clippy::cast_ptr_alignment)]
            let inner = new_ptr.cast::<SharedVec>();
            (*inner).capacity = (new_layout.size() - METADATA_SIZE) as u32;
            self.0 = NonNull::new_unchecked(inner);
        }
    }

    #[inline]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        let capacity = self.capacity();
        assert!(len <= capacity);
        (*self.as_inner()).len = len as u32;
    }

    /// Moves the start of the view forward by `start` bytes.
    ///
    /// `start` is checked against the view length as `usize`, before it is
    /// narrowed to the `u32` header fields, so values of 4 GiB or more
    /// cannot wrap around.
    ///
    /// # Panics
    ///
    /// Panics if `start` is greater than the view length.
    pub(crate) unsafe fn set_start(&mut self, start: usize) {
        if start != 0 {
            let inner = self.as_inner();

            assert!(
                start <= (*inner).len as usize,
                "cannot advance past the end of the buffer, cnt:{start} len:{}",
                (*inner).len,
            );
            let start = start as u32;

            // Updating the start of the view is setting `offset` to point to the
            // new start and updating the `len` field to reflect the new length
            // of the view.
            // `remaining` does not change, the view capacity shrinks by the
            // same amount as the length.
            (*inner).offset += start;
            (*inner).len -= start;
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

thread_local! {
    static CACHE: Cell<Option<Box<Cache>>> = Cell::new(Some(Box::default()));
}

pub(crate) fn set_pages_cache(size: usize) {
    let _ = CACHE.try_with(|c| {
        if let Some(mut cst) = c.take() {
            cst.limits = [size; PAGE_CLASSES];
            c.set(Some(cst));
        }
    });
}

pub(crate) fn set_page_cache_size(size: BytePageSize, count: usize) {
    if size == BytePageSize::Unset {
        return;
    }
    let _ = CACHE.try_with(|c| {
        if let Some(mut cst) = c.take() {
            cst.limits[size as usize] = count;
            c.set(Some(cst));
        }
    });
}

/// Number of page categories, `BytePageSize::Unset` excluded.
const PAGE_CLASSES: usize = BytePageSize::Unset as usize;

/// Default number of cached pages per page size, fewer pages are cached
/// for larger sizes. Reads start at 4 KiB pages and adapt up to 64 KiB
/// pages under load, 16 KiB is the default page size of writes.
const DEFAULT_PAGES_CACHE: [usize; PAGE_CLASSES] = [128, 64, 64, 32, 16, 8, 16, 2, 1];

struct Cache {
    limits: [usize; PAGE_CLASSES],
    cache: [Vec<StorageVec>; PAGE_CLASSES],
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            limits: DEFAULT_PAGES_CACHE,
            cache: Default::default(),
        }
    }
}

impl SharedVec {
    pub(crate) fn create(cap: usize, src: &[u8]) -> NonNull<SharedVec> {
        assert!(
            cap >= src.len(),
            "SharedVec capacity {cap} is smaller than data length {}",
            src.len()
        );
        let ptr = Self::alloc_with_capacity(cap, src.len() as u32);

        // copy slice
        unsafe {
            let dst = ptr.add(METADATA_SIZE);
            let sl = slice::from_raw_parts_mut(dst, src.len());
            sl.copy_from_slice(src);
            #[allow(clippy::cast_ptr_alignment)]
            NonNull::new_unchecked(ptr.cast::<SharedVec>())
        }
    }

    fn alloc_with_capacity(cap: usize, len: u32) -> *mut u8 {
        assert!(
            cap <= MAX_CAPACITY,
            "buffer capacity {cap} exceeds maximum {MAX_CAPACITY}"
        );
        let layout = shared_vec_layout(cap).unwrap();

        // Alloc memory and store data
        unsafe {
            let ptr = alloc::alloc(layout);
            if ptr.is_null() {
                alloc::handle_alloc_error(layout);
            }
            let capacity = (layout.size() - METADATA_SIZE) as u32;

            #[cfg(feature = "overuse")]
            if cap > 1081344 {
                log::debug!("Buffer size {capacity}\n{:?}", backtrace::Backtrace::new());
            }

            #[allow(clippy::cast_ptr_alignment)]
            ptr::write(
                ptr.cast::<SharedVec>(),
                SharedVec {
                    len,
                    capacity,
                    offset: METADATA_SIZE_U32,
                    ref_count: AtomicU32::new(1),
                },
            );
            ptr
        }
    }

    fn is_unique(&self) -> bool {
        // Acquire synchronizes with the Release decrement of other handles
        self.ref_count.load(Acquire) == 1
    }

    /// Returns the data capacity of the allocation.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `SharedVec`. Only the `capacity` field is
    /// read, so this is safe to call while the `BytesMut` handle is in use.
    pub(crate) unsafe fn capacity(ptr: *const SharedVec) -> usize {
        ptr::addr_of!((*ptr).capacity).read() as usize
    }
}

pub(crate) fn release_shared_vec(ptr: *mut SharedVec) {
    // follow the drop steps from Arc
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
        // [1]: https://www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html
        atomic::fence(Acquire);

        let capacity = (*ptr).capacity;

        // Try to put to cache, any allocation of a page size is a page
        let size = BytePageSize::from_alloc_size(capacity as usize + METADATA_SIZE);
        if size != BytePageSize::Unset {
            // the cache is unavailable while the thread-local is being destroyed,
            // the page is freed instead
            let cached = CACHE.try_with(|c| {
                let Some(mut cst) = c.take() else {
                    return false;
                };
                let res = if cst.cache[size as usize].len() < cst.limits[size as usize] {
                    (*ptr).len = 0;
                    (*ptr).offset = METADATA_SIZE_U32;
                    (*ptr).ref_count = AtomicU32::new(1);
                    cst.cache[size as usize].push(StorageVec(NonNull::new_unchecked(ptr)));
                    true
                } else {
                    false
                };
                c.set(Some(cst));
                res
            });
            if matches!(cached, Ok(true)) {
                return;
            }
        }

        // Drop the data
        ptr::drop_in_place(ptr);
        let layout = shared_vec_layout(capacity as usize).unwrap();
        alloc::dealloc(ptr.cast(), layout);
    }
}

/// The capacity a buffer of `len` bytes grows to when it needs `required`.
///
/// It is at least twice the length, so appending in small steps reallocates a
/// logarithmic number of times, a buffer holding little data still gets what
/// it asked for.
fn grown_capacity(len: usize, required: usize) -> usize {
    cmp::max(required, cmp::min(len.saturating_mul(2), MAX_CAPACITY))
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

#[inline(never)]
#[cold]
pub(crate) fn abort() -> ! {
    std::process::abort()
}

#[cfg(test)]
#[allow(clippy::assert_is_empty)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn cached() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        let mut st = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(st.page_size(), BytePageSize::Size8);

        st.put_u8(b'h');
        let addr = st.0;
        drop(st);

        let st = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(addr, st.0);
    }

    #[test]
    fn default_cache_limit_per_page_size() {
        assert_eq!(
            super::DEFAULT_PAGES_CACHE,
            [128, 64, 64, 32, 16, 8, 16, 2, 1]
        );
        for size in crate::size::PAGE_SIZES {
            super::CACHE.with(|cache| cache.set(Some(Box::default())));

            let limit = super::DEFAULT_PAGES_CACHE[size as usize];
            let pages: Vec<_> = (0..limit + 4).map(|_| StorageVec::sized(size)).collect();
            drop(pages);
            assert_eq!(cached_pages(size), limit, "{size:?}");
        }
        super::CACHE.with(|cache| cache.set(Some(Box::default())));
    }

    #[test]
    fn page_allocation_is_category_size() {
        for (size, alloc) in [
            (BytePageSize::Size4, 4 * 1024),
            (BytePageSize::Size16, 16 * 1024),
            (BytePageSize::Size64, 64 * 1024),
            (BytePageSize::Size128, 128 * 1024),
            (BytePageSize::Size256, 256 * 1024),
        ] {
            let st = StorageVec::sized(size);
            assert_eq!(st.capacity(), size.capacity());
            let layout = shared_vec_layout(st.capacity()).unwrap();
            assert_eq!(layout.size(), alloc, "{size:?}");
        }
    }

    // Run under miri: without `Acquire`, the write below races with the read
    // made by the other thread before it released its handle.
    #[test]
    fn is_unique_synchronizes_with_release() {
        let mut st = StorageVec::with_capacity(64);
        for _ in 0..=INLINE_CAP {
            st.put_u8(1);
        }
        let other = st.shallow_freeze();
        assert!(!other.is_inline());
        let handle = std::thread::spawn(move || {
            let val = other.as_ref()[0];
            drop(other);
            val
        });

        while !st.is_unique() {
            std::thread::yield_now();
        }
        st.as_mut()[0] = 2;
        assert_eq!(handle.join().unwrap(), 1);
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    #[test]
    fn reserve_grows_unique_buffer() {
        let data = pattern(100);
        let mut st = StorageVec::from_slice(100, &data);
        st.reserve(1000);
        assert_eq!(st.as_ref(), &data[..]);
        assert!(st.capacity() >= 1100);
        assert_eq!(st.remaining(), st.capacity() - st.len());

        // the view may start past the allocation start
        let mut st = StorageVec::from_slice(100, &data);
        unsafe { st.set_start(30) };
        st.reserve(1000);
        assert_eq!(st.as_ref(), &data[30..]);
        assert!(st.capacity() >= 1070);
        assert_eq!(st.remaining(), st.capacity() - st.len());
        for i in 0..1000 {
            st.put_u8(i as u8);
        }
        assert_eq!(&st.as_ref()[..70], &data[30..]);
        assert_eq!(st.len(), 1070);

        // nothing left in the view
        let mut st = StorageVec::from_slice(100, &data);
        unsafe { st.set_start(100) };
        st.reserve(1000);
        assert!(st.as_ref().is_empty());
        assert_eq!(st.remaining(), st.capacity());
    }

    #[test]
    fn reserve_keeps_shared_views() {
        let data = pattern(100);
        let mut st = StorageVec::from_slice(100, &data);
        let view = st.shallow_freeze();
        assert!(!view.is_inline());
        st.reserve(1000);
        st.as_mut()[0] = 0xff;
        assert_eq!(view.as_ref(), &data[..]);
        assert_eq!(&st.as_ref()[1..], &data[1..]);
    }

    #[test]
    fn reserve_pooled_page_grows_to_next_page_size() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        let mut st = StorageVec::sized(BytePageSize::Size8);
        let page = st.0;
        let data = pattern(st.capacity());
        for b in &data {
            st.put_u8(*b);
        }
        st.reserve(1);
        assert_eq!(st.page_size(), BytePageSize::Size16);
        assert_eq!(st.capacity(), BytePageSize::Size16.capacity());
        assert_ne!(st.0, page);
        assert_eq!(st.as_ref(), &data[..]);

        // the page went back to the cache with its size class
        assert_eq!(cached_pages(BytePageSize::Size8), 1);
        let st2 = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(st2.0, page);
        drop(st);
        assert_eq!(cached_pages(BytePageSize::Size16), 1);
    }

    #[test]
    fn reserve_pooled_page_skips_page_sizes() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        let data = pattern(100);
        let mut st = StorageVec::sized(BytePageSize::Size4);
        assert_eq!(st.put_slice_partial(&data), data.len());
        st.reserve(40 * 1024);
        assert_eq!(st.page_size(), BytePageSize::Size48);
        assert_eq!(st.as_ref(), &data[..]);

        // a small reservation keeps at least the current page size
        let mut st = StorageVec::sized(BytePageSize::Size32);
        let full = pattern(st.capacity());
        assert_eq!(st.put_slice_partial(&full), full.len());
        unsafe { st.set_start(full.len() - 100) };
        let view = st.shallow_freeze();
        assert_eq!(st.remaining(), 0);
        st.reserve(10);
        assert_eq!(st.page_size(), BytePageSize::Size32);
        assert_eq!(st.as_ref(), &full[full.len() - 100..]);
        assert_eq!(view.as_ref(), &full[full.len() - 100..]);
    }

    #[test]
    fn reserve_shared_pooled_page() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        let mut st = StorageVec::sized(BytePageSize::Size4);
        let data = pattern(st.capacity());
        assert_eq!(st.put_slice_partial(&data), data.len());
        let view = st.shallow_freeze();
        st.reserve(1);
        assert_eq!(st.page_size(), BytePageSize::Size8);
        st.as_mut()[0] = 0xff;
        assert_eq!(view.as_ref(), &data[..]);
        assert_eq!(&st.as_ref()[1..], &data[1..]);

        // the old page is cached when the last view is dropped
        assert_eq!(cached_pages(BytePageSize::Size4), 0);
        drop(view);
        assert_eq!(cached_pages(BytePageSize::Size4), 1);
    }

    #[test]
    fn reserve_pooled_page_above_largest_page_size() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        let data = pattern(1000);
        let mut st = StorageVec::sized(BytePageSize::Size256);
        assert_eq!(st.put_slice_partial(&data), data.len());
        st.reserve(BytePageSize::Size256.capacity());
        assert_eq!(st.page_size(), BytePageSize::Unset);
        assert_eq!(st.capacity(), 1000 + BytePageSize::Size256.capacity());
        assert_eq!(st.as_ref(), &data[..]);
        assert_eq!(cached_pages(BytePageSize::Size256), 1);

        // without a page size it is freed, not cached
        drop(st);
        for size in crate::size::PAGE_SIZES {
            assert_eq!(
                cached_pages(size),
                usize::from(size == BytePageSize::Size256)
            );
        }
    }

    #[test]
    fn reserve_exact_pooled_page() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        // a unique page is reallocated to the exact capacity
        let data = pattern(100);
        let mut st = StorageVec::sized(BytePageSize::Size4);
        assert_eq!(st.put_slice_partial(&data), data.len());
        st.reserve_exact(5000);
        assert_eq!(st.capacity(), 5100);
        assert_eq!(st.page_size(), BytePageSize::Unset);
        assert_eq!(st.as_ref(), &data[..]);
        assert_eq!(cached_pages(BytePageSize::Size4), 0);

        // a shared page is copied into an exact allocation
        let mut st = StorageVec::sized(BytePageSize::Size4);
        assert_eq!(st.put_slice_partial(&data), data.len());
        let view = st.shallow_freeze();
        st.reserve_exact(5000);
        assert_eq!(st.capacity(), 5100);
        assert_eq!(st.page_size(), BytePageSize::Unset);
        assert_eq!(st.as_ref(), &data[..]);
        assert_eq!(view.as_ref(), &data[..]);
        drop(view);
        assert_eq!(cached_pages(BytePageSize::Size4), 1);

        // an exact page capacity is a page
        let mut st = StorageVec::sized(BytePageSize::Size4);
        assert_eq!(st.put_slice_partial(&data), data.len());
        st.reserve_exact(BytePageSize::Size8.capacity() - 100);
        assert_eq!(st.page_size(), BytePageSize::Size8);
        assert_eq!(st.as_ref(), &data[..]);
    }

    #[test]
    fn reserve_unsized_buffer_keeps_page_size() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        // the capacity is not rounded up to a page size
        let mut st = StorageVec::with_capacity(100);
        assert_eq!(st.put_slice_partial(&pattern(100)), 100);
        st.reserve_exact(4000);
        assert_eq!(st.page_size(), BytePageSize::Unset);
        assert_eq!(st.capacity(), 4100);

        // shared buffer gets a new buffer without a page size
        let view = st.shallow_freeze();
        st.reserve(st.remaining() + 1);
        assert_eq!(st.page_size(), BytePageSize::Unset);
        drop(view);

        // an allocation of a page size is a page, whichever way it was created
        let st = StorageVec::with_capacity(BytePageSize::Size4.capacity());
        assert_eq!(st.capacity(), BytePageSize::Size4.capacity());
        assert_eq!(st.page_size(), BytePageSize::Size4);
        drop(st);
        assert_eq!(cached_pages(BytePageSize::Size4), 1);
        let st = StorageVec::from_slice(BytePageSize::Size8.capacity(), b"hello");
        assert_eq!(st.page_size(), BytePageSize::Size8);
        drop(st);
        assert_eq!(cached_pages(BytePageSize::Size8), 1);

        // the cached page is reset
        let mut st = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(st.len(), 0);
        assert_eq!(st.remaining(), BytePageSize::Size8.capacity());
        assert_eq!(st.put_slice_partial(b"world"), 5);
        assert_eq!(st.as_ref(), b"world");
    }

    #[test]
    fn remaining_is_derived() {
        let mut st = StorageVec::with_capacity(100);
        let cap = st.capacity();
        assert_eq!(st.remaining(), cap);
        assert!(!st.is_full());

        st.put_u8(1);
        assert_eq!(st.put_slice_partial(&pattern(50)), 50);
        assert_eq!(st.remaining(), cap - 51);

        // advancing the start keeps the spare capacity
        unsafe { st.set_start(10) };
        assert_eq!(st.len(), 41);
        assert_eq!(st.capacity(), cap - 10);
        assert_eq!(st.remaining(), cap - 51);

        st.truncate(20);
        assert_eq!(st.remaining(), cap - 30);

        // reclaiming a unique buffer resets the view
        st.truncate(0);
        assert_eq!(st.capacity(), cap);
        assert_eq!(st.remaining(), cap);

        assert_eq!(st.put_slice_partial(&pattern(cap + 10)), cap);
        assert_eq!(st.remaining(), 0);
        assert!(st.is_full());

        // a unique frozen view becomes the `BytesMut` view
        let mut b = BytesMut::with_capacity(300);
        b.extend_from_slice(&pattern(200));
        let cap = b.capacity();
        b.advance(50);
        let ptr = b.as_ptr();
        let mut b = BytesMut::from(b.freeze());
        assert_eq!(b.as_ptr(), ptr);
        assert_eq!(b.len(), 150);
        assert_eq!(BufMut::remaining_mut(&b), cap - 200);
        b.extend_from_slice(&pattern(cap - 200));
        assert_eq!(BufMut::remaining_mut(&b), 0);
    }

    fn cached_pages(size: BytePageSize) -> usize {
        super::CACHE.with(|c| {
            let cst = c.take().unwrap();
            let len = cst.cache[size as usize].len();
            c.set(Some(cst));
            len
        })
    }

    #[test]
    #[allow(deprecated)]
    fn pages_cache_size() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        crate::set_pages_cache(1);
        drop((
            StorageVec::sized(BytePageSize::Size4),
            StorageVec::sized(BytePageSize::Size4),
        ));
        assert_eq!(cached_pages(BytePageSize::Size4), 1);

        crate::set_pages_cache(0);
        drop(StorageVec::sized(BytePageSize::Size8));
        assert_eq!(cached_pages(BytePageSize::Size8), 0);

        // the cache is in use, the page is freed
        crate::set_pages_cache(16);
        let st = StorageVec::sized(BytePageSize::Size16);
        let cache = super::CACHE.with(Cell::take);
        drop(st);
        super::CACHE.with(|c| c.set(cache));
        assert_eq!(cached_pages(BytePageSize::Size16), 0);

        // the setting is ignored while the cache is in use
        let cache = super::CACHE.with(Cell::take);
        crate::set_pages_cache(3);
        super::CACHE.with(|c| c.set(cache));
        assert_eq!(cache_limits(), [16; super::PAGE_CLASSES]);
    }

    fn cache_limits() -> [usize; super::PAGE_CLASSES] {
        super::CACHE.with(|c| {
            let cst = c.take().unwrap();
            let limits = cst.limits;
            c.set(Some(cst));
            limits
        })
    }

    #[test]
    #[allow(deprecated)]
    fn page_cache_size() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        crate::set_page_cache_size(BytePageSize::Size64, 1);
        crate::set_page_cache_size(BytePageSize::Unset, 5);
        let mut expected = super::DEFAULT_PAGES_CACHE;
        expected[BytePageSize::Size64 as usize] = 1;
        assert_eq!(cache_limits(), expected);

        drop((
            StorageVec::sized(BytePageSize::Size64),
            StorageVec::sized(BytePageSize::Size64),
            StorageVec::sized(BytePageSize::Size48),
            StorageVec::sized(BytePageSize::Size48),
        ));
        assert_eq!(cached_pages(BytePageSize::Size64), 1);
        assert_eq!(cached_pages(BytePageSize::Size48), 2);

        // the setting is ignored while the cache is in use
        let cache = super::CACHE.with(Cell::take);
        crate::set_page_cache_size(BytePageSize::Size48, 3);
        super::CACHE.with(|c| c.set(cache));
        assert_eq!(cache_limits(), expected);

        crate::set_pages_cache(2);
        assert_eq!(cache_limits(), [2; super::PAGE_CLASSES]);
        super::CACHE.with(|cache| cache.set(Some(Box::default())));
    }

    #[test]
    fn bytes_mut_with_page_size() {
        super::CACHE.with(|cache| cache.set(Some(Box::default())));

        let mut buf = BytesMut::with_page_size(BytePageSize::Size8);
        assert_eq!(buf.page_size(), BytePageSize::Size8);
        assert_eq!(buf.capacity(), BytePageSize::Size8.capacity());
        let ptr = buf.as_ptr();

        // split off data keeps the page until the last reference is dropped
        buf.extend_from_slice(&[1; 1000]);
        let head = buf.split_to(500);
        drop(buf);
        assert_eq!(cached_pages(BytePageSize::Size8), 0);
        drop(head);
        assert_eq!(cached_pages(BytePageSize::Size8), 1);

        let buf = BytesMut::with_page_size(BytePageSize::Size8);
        assert_eq!(buf.as_ptr(), ptr);
        assert_eq!(buf.capacity(), BytePageSize::Size8.capacity());
        assert_eq!(cached_pages(BytePageSize::Size8), 0);

        // `Unset` creates a `Size64` page
        let buf = BytesMut::with_page_size(BytePageSize::Unset);
        assert_eq!(buf.page_size(), BytePageSize::Size64);
        assert_eq!(buf.capacity(), BytePageSize::Size64.capacity());
        drop(buf);
        assert_eq!(cached_pages(BytePageSize::Size64), 1);

        // buffers without a page size are not cached
        let buf = BytesMut::with_capacity(BytePageSize::Size64.capacity() + 1);
        assert_eq!(buf.page_size(), BytePageSize::Unset);
        assert_eq!(BytesMut::new().page_size(), BytePageSize::Unset);
        assert_eq!(BytesMut::with_capacity(64).page_size(), BytePageSize::Unset);
        assert_eq!(
            BytesMut::copy_from_slice(b"hello").page_size(),
            BytePageSize::Unset
        );
        drop(buf);
        for size in crate::size::PAGE_SIZES {
            let expected = usize::from(size == BytePageSize::Size64);
            assert_eq!(cached_pages(size), expected);
        }
    }

    #[test]
    fn truncate_reclaims_unique_buffer() {
        let mut b = BytesMut::with_capacity(128);
        let cap = b.capacity();
        b.extend_from_slice(&[1; 64]);
        b.advance(0);
        b.advance(32);
        assert_eq!(b.capacity(), cap - 32);
        b.truncate(0);
        assert_eq!(b.capacity(), cap);

        // a shared buffer is not reclaimed
        b.extend_from_slice(&[1; 64]);
        b.advance(32);
        let other = b.split_to(30);
        b.truncate(0);
        assert_eq!(b.capacity(), cap - 62);
        drop(other);
    }

    #[test]
    fn resize_shrinks() {
        let mut b = BytesMut::copy_from_slice(b"hello world");
        b.resize(5, 0);
        assert_eq!(&b[..], b"hello");
        b.resize(7, b'!');
        assert_eq!(&b[..], b"hello!!");
    }

    #[test]
    fn reserve_reclaims_front_space() {
        let mut b = BytesMut::with_capacity(128);
        let cap = b.capacity();
        b.extend_from_slice(&[1; 40]);
        b.extend_from_slice(&[2; 10]);
        b.advance(40);
        let spare = b.capacity() - b.len();

        // the data is moved to the start of the allocation
        b.reserve(spare + 1);
        assert_eq!(&b[..], &[2; 10]);
        assert_eq!(b.capacity(), cap);
        assert!(b.is_unique());
    }

    #[test]
    fn reserve_exact() {
        let mut b = BytesMut::with_capacity(64);
        b.extend_from_slice(&[1; 64]);
        let cap = b.capacity();

        b.reserve_exact(0);
        assert_eq!(b.capacity(), cap);

        // grows a unique buffer in place, to the exact size
        b.reserve_exact(1000);
        assert_eq!(&b[..], &[1; 64]);
        assert!(b.capacity() >= 1064);
        assert!(b.capacity() < 1064 + 2 * METADATA_SIZE);

        // a shared buffer is copied
        let other = b.split_to(32);
        b.reserve_exact(2000);
        assert_eq!(&b[..], &[1; 32]);
        assert!(b.capacity() >= 2032);
        assert!(b.capacity() < 2032 + 2 * METADATA_SIZE);
        assert!(b.is_unique());
        assert_eq!(&other[..], &[1; 32]);
    }
}
