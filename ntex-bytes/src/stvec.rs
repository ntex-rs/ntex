use crate::alloc::alloc::{self, Layout, LayoutError};

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{self, AtomicU32};
use std::{cmp, mem, ptr, ptr::NonNull, slice};

use crate::storage::{INLINE_CAP, MIN_CAPACITY, Storage};
#[cfg(feature = "tracker")]
use crate::tracker::{BytesTracker, TrackerInner};
use crate::{BytePageSize, buf::UninitSlice, cache};

#[derive(Debug)]
/// Thread-safe reference-counted container for the shared storage.
pub(crate) struct Header {
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
    /// Tracker charged with the allocation, null if not attributed.
    #[cfg(feature = "tracker")]
    pub(crate) tracker: *const TrackerInner,
    /// Keeps the data 16-byte aligned, a misaligned start slows down copies.
    #[cfg(all(feature = "tracker", target_pointer_width = "64"))]
    _pad: u64,
}

#[derive(Debug)]
pub(crate) struct StorageVec(pub(crate) NonNull<Header>);

/// Size of the header stored in front of the data of every heap buffer.
pub const METADATA_SIZE: usize = mem::size_of::<Header>();

#[cfg(not(feature = "tracker"))]
const _: () = assert!(METADATA_SIZE == 16 && mem::align_of::<Header>() == 4);
#[cfg(all(feature = "tracker", target_pointer_width = "64"))]
const _: () = assert!(METADATA_SIZE == 32 && mem::align_of::<Header>() == 8);
#[cfg(all(feature = "tracker", target_pointer_width = "32"))]
const _: () = assert!(METADATA_SIZE == 20 && mem::align_of::<Header>() == 4);
const METADATA_SIZE_U32: u32 = METADATA_SIZE as u32;

/// Maximum buffer capacity, offsets and sizes are stored as `u32`.
pub(crate) const MAX_CAPACITY: usize = u32::MAX as usize - METADATA_SIZE;

/// Largest capacity step of `reserve_more` for buffers without a page size.
const MAX_MORE_STEP: usize = 64 * 1024;

impl StorageVec {
    /// Create new empty storage with specified capacity
    pub(crate) fn with_capacity(capacity: usize) -> StorageVec {
        StorageVec(Header::create(capacity, &[]))
    }

    /// Create new empty storage with specified size category
    pub(crate) fn sized(size: BytePageSize) -> StorageVec {
        cache::take_page(size).unwrap_or_else(|| StorageVec(Header::create(size.capacity(), &[])))
    }

    /// Create new empty storage with specified size category, charged
    /// to `tracker`.
    #[cfg(feature = "tracker")]
    pub(crate) fn sized_in(size: BytePageSize, tracker: &BytesTracker) -> StorageVec {
        let st = StorageVec::sized(size);
        // SAFETY: the page is new or recycled, the handle keeps the tracker alive
        unsafe { Header::attach(st.0.as_ptr(), tracker.as_inner()) };
        st
    }

    /// Returns the tracker charged with the allocation.
    #[cfg(feature = "tracker")]
    pub(crate) fn tracker(&self) -> Option<&BytesTracker> {
        let ptr = self.0.as_ptr();
        // SAFETY: `self` holds a reference and is the only handle that writes
        // the field while the allocation is shared, which needs `&mut self`.
        // `BytesTracker` is a transparent non-null pointer and the charge
        // keeps the tracker alive.
        unsafe {
            if Header::tracker(ptr).is_null() {
                None
            } else {
                Some(&*ptr::addr_of!((*ptr).tracker).cast::<BytesTracker>())
            }
        }
    }

    /// Charges the allocation to `tracker`, moving the charge from the
    /// current tracker if any.
    #[cfg(feature = "tracker")]
    pub(crate) fn set_tracker(&mut self, tracker: &BytesTracker) {
        let ptr = self.0.as_ptr();
        let new = tracker.as_inner();
        // SAFETY: `self` holds a reference and is the only handle that writes
        // the field while the allocation is shared, see `Header::tracker`.
        // The new tracker is charged before the old one is released, the
        // handle keeps `tracker` alive.
        unsafe {
            let current = Header::tracker(ptr);
            if current != new {
                let size = Header::capacity(ptr) + METADATA_SIZE;
                (*new).charge(size);
                ptr::addr_of_mut!((*ptr).tracker).write(new);
                if !current.is_null() {
                    TrackerInner::uncharge(current, size);
                }
            }
        }
    }

    /// Charges the new storage `st` to the tracker of `self`.
    #[inline]
    #[cfg_attr(not(feature = "tracker"), allow(unused_variables, clippy::unused_self))]
    fn inherit_tracker(&self, st: &StorageVec) {
        // SAFETY: `st` is a new allocation, `self` holds a reference to its tracker
        #[cfg(feature = "tracker")]
        unsafe {
            Header::attach(st.0.as_ptr(), Header::tracker(self.0.as_ptr()));
        }
    }

    /// Resets the header of a released page for reuse.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `Header` without references.
    pub(crate) unsafe fn recycle(ptr: *mut Header) -> StorageVec {
        unsafe {
            (*ptr).len = 0;
            (*ptr).offset = METADATA_SIZE_U32;
            (*ptr).ref_count = AtomicU32::new(1);
            #[cfg(feature = "tracker")]
            {
                (*ptr).tracker = ptr::null();
            }
            StorageVec(NonNull::new_unchecked(ptr))
        }
    }

    /// Create new storage with capacity and copy slice
    ///
    /// Panics if `capacity` is smaller than `src` length
    pub(crate) fn from_slice(capacity: usize, src: &[u8]) -> StorageVec {
        StorageVec(Header::create(capacity, src))
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
    /// A `&mut Header` must not be created, other handles access
    /// `ref_count` and `capacity` concurrently.
    fn as_inner(&mut self) -> *mut Header {
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
        unsafe { Header::is_unique(self.0.as_ptr()) }
    }

    /// Takes ownership of the allocation of a frozen view, if the view holds
    /// the only reference to it.
    ///
    /// The view becomes the `BytesMut` view, the rest of the allocation past
    /// it is spare capacity.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `Header` referenced by the caller, the
    /// view `offset..offset + len` must be within the allocation. On success
    /// the caller's reference is transferred to the returned handle.
    pub(crate) unsafe fn from_unique_view(
        ptr: *mut Header,
        offset: usize,
        len: usize,
    ) -> Option<StorageVec> {
        // `Acquire` synchronizes with the `Release` decrement of the handles
        // dropped by other threads, their accesses happen before the header
        // is updated below.
        if !Header::is_unique(ptr) {
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

                Storage::from_shared_vec(self.0, inner.offset as usize, self.len())
            }
        }
    }

    pub(crate) fn freeze(self) -> Storage {
        unsafe {
            if self.len() <= INLINE_CAP {
                Storage::from_ptr_inline(self.as_ptr(), self.len())
            } else {
                let offset = self.0.as_ref().offset as usize;
                let inner = Storage::from_shared_vec(self.0, offset, self.len());
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

                Storage::from_shared_vec(self.0, (*inner).offset as usize, at)
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
                if Header::is_unique(inner) && (*inner).offset != METADATA_SIZE_U32 {
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
        if len + half <= unsafe { Header::capacity(self.0.as_ptr()) } {
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
        self.inherit_tracker(&st);
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

            if Header::is_unique(inner) {
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

            let st = StorageVec(Header::create(grow_cap, self.as_ref()));
            self.inherit_tracker(&st);
            *self = st;
        }
    }

    /// Returns the page category of the buffer.
    ///
    /// It is derived from the allocation size, any allocation of a page
    /// size belongs to the page category.
    pub(crate) fn page_size(&self) -> BytePageSize {
        unsafe { BytePageSize::from_alloc_size(Header::capacity(self.0.as_ptr()) + METADATA_SIZE) }
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
            let inner = new_ptr.cast::<Header>();
            (*inner).capacity = (new_layout.size() - METADATA_SIZE) as u32;
            self.0 = NonNull::new_unchecked(inner);

            #[cfg(feature = "tracker")]
            {
                let tracker = (*inner).tracker;
                if !tracker.is_null() {
                    (*tracker).resize(old_layout.size(), new_layout.size());
                }
            }
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

impl Header {
    pub(crate) fn create(cap: usize, src: &[u8]) -> NonNull<Header> {
        assert!(
            cap >= src.len(),
            "Header capacity {cap} is smaller than data length {}",
            src.len()
        );
        let ptr = Self::alloc_with_capacity(cap, src.len() as u32);

        // copy slice
        unsafe {
            let dst = ptr.add(METADATA_SIZE);
            let sl = slice::from_raw_parts_mut(dst, src.len());
            sl.copy_from_slice(src);
            #[allow(clippy::cast_ptr_alignment)]
            NonNull::new_unchecked(ptr.cast::<Header>())
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

            #[allow(clippy::cast_ptr_alignment)]
            ptr::write(
                ptr.cast::<Header>(),
                Header {
                    len,
                    capacity,
                    offset: METADATA_SIZE_U32,
                    ref_count: AtomicU32::new(1),
                    #[cfg(feature = "tracker")]
                    tracker: ptr::null(),
                    #[cfg(all(feature = "tracker", target_pointer_width = "64"))]
                    _pad: 0,
                },
            );
            ptr
        }
    }

    /// Returns `true` if the caller holds the only reference.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `Header` referenced by the caller. Only
    /// `ref_count` is borrowed, the `BytesMut` handle may write other
    /// fields concurrently.
    unsafe fn is_unique(ptr: *const Header) -> bool {
        // Acquire synchronizes with the Release decrement of other handles
        unsafe { (*ptr).ref_count.load(Acquire) == 1 }
    }

    /// Returns the tracker charged with the allocation, null if none.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `Header` referenced by the caller. The
    /// `BytesMut` handle writes the field even while the allocation is
    /// shared, so the caller must be the `BytesMut` handle, hold the only
    /// reference, or be the final release.
    #[cfg(feature = "tracker")]
    pub(crate) unsafe fn tracker(ptr: *const Header) -> *const TrackerInner {
        unsafe { ptr::addr_of!((*ptr).tracker).read() }
    }

    /// Charges the allocation to `tracker`, a null tracker is ignored.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live untracked `Header` with a single
    /// reference. `tracker` must be null or point to a live tracker.
    #[cfg(feature = "tracker")]
    pub(crate) unsafe fn attach(ptr: *mut Header, tracker: *const TrackerInner) {
        if !tracker.is_null() {
            unsafe {
                debug_assert!((*ptr).tracker.is_null());
                (*tracker).charge((*ptr).capacity as usize + METADATA_SIZE);
                (*ptr).tracker = tracker;
            }
        }
    }

    /// Returns the data capacity of the allocation.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a live `Header`. Only the `capacity` field is
    /// read, so this is safe to call while the `BytesMut` handle is in use.
    pub(crate) unsafe fn capacity(ptr: *const Header) -> usize {
        ptr::addr_of!((*ptr).capacity).read() as usize
    }
}

pub(crate) fn release_shared_vec(ptr: *mut Header) {
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

        // cached pages are not attributed
        #[cfg(feature = "tracker")]
        {
            let tracker = ptr::replace(ptr::addr_of_mut!((*ptr).tracker), ptr::null());
            if !tracker.is_null() {
                TrackerInner::uncharge(tracker, capacity as usize + METADATA_SIZE);
            }
        }

        // Try to put to cache, any allocation of a page size is a page
        let size = BytePageSize::from_alloc_size(capacity as usize + METADATA_SIZE);
        if size != BytePageSize::Unset && cache::put_page(ptr, size) {
            return;
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
    match Layout::new::<Header>().pad_to_align().extend(s_layout) {
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

    /// STATEMENT: Page storage allocation layouts match their size categories including metadata.
    ///
    /// HOW:
    /// - Allocate `Size4`, `Size16`, `Size64`, `Size128`, and `Size256` storage and check usable capacities and full layout sizes.
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
    /// STATEMENT: Observing unique storage synchronizes mutation with a different thread releasing its view.
    ///
    /// HOW:
    /// - Shallow-freeze non-inline data, read and drop that view on another thread, and wait until `is_unique` succeeds.
    /// - Mutate the reclaimed storage and verify the joined thread read the original byte.
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

    /// STATEMENT: Reserving unique storage preserves the visible bytes and provides the requested writable space.
    ///
    /// HOW:
    /// - Reserve 1,000 additional bytes for a patterned buffer, an offset view, and an empty view, checking contents and capacity accounting.
    /// - Fill the offset view's reserved space and verify its original prefix and final length.
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

    /// STATEMENT: Growing shared storage preserves independent immutable views of its original bytes.
    ///
    /// HOW:
    /// - Shallow-freeze a 100-byte pattern, reserve 1,000 bytes, mutate the grown buffer, and verify the old view and untouched suffix.
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

    /// STATEMENT: Growing a full pooled page selects the next size class and returns the old page to its cache.
    ///
    /// HOW:
    /// - Fill `Size8`, reserve one byte, and check `Size16` capacity, changed storage identity, and preserved contents.
    /// - Reallocate `Size8` and check reuse of the old address, then drop the grown page and check its class cache.
    #[test]
    fn reserve_pooled_page_grows_to_next_page_size() {
        cache::reset();

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
        assert_eq!(cache::cached_pages(BytePageSize::Size8), 1);
        let st2 = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(st2.0, page);
        drop(st);
        assert_eq!(cache::cached_pages(BytePageSize::Size16), 1);
    }

    /// STATEMENT: Pooled reservation skips to a fitting size class but never shrinks the current class.
    ///
    /// HOW:
    /// - Reserve 40 KiB after 100 bytes in `Size4` and check selection of `Size48` with unchanged data.
    /// - Retain a shared 100-byte tail of full `Size32`, reserve ten bytes, and verify the class and both views' contents.
    #[test]
    fn reserve_pooled_page_skips_page_sizes() {
        cache::reset();

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

    /// STATEMENT: Growing a shared pooled page copies data and delays caching the old page until its last view drops.
    ///
    /// HOW:
    /// - Fill and shallow-freeze `Size4`, reserve one byte, mutate the resulting `Size8`, and check the old view remains intact.
    /// - Check the `Size4` cache before and after dropping that view.
    #[test]
    fn reserve_shared_pooled_page() {
        cache::reset();

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
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 0);
        drop(view);
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 1);
    }

    /// STATEMENT: A reservation exceeding all page classes falls back to uncached ordinary storage.
    ///
    /// HOW:
    /// - Reserve a `Size256` capacity after 1,000 initialized bytes and check `Unset`, exact capacity, and preserved data.
    /// - Verify the old page is cached and dropping the oversized buffer adds no cached pages.
    #[test]
    fn reserve_pooled_page_above_largest_page_size() {
        cache::reset();

        let data = pattern(1000);
        let mut st = StorageVec::sized(BytePageSize::Size256);
        assert_eq!(st.put_slice_partial(&data), data.len());
        st.reserve(BytePageSize::Size256.capacity());
        assert_eq!(st.page_size(), BytePageSize::Unset);
        assert_eq!(st.capacity(), 1000 + BytePageSize::Size256.capacity());
        assert_eq!(st.as_ref(), &data[..]);
        assert_eq!(cache::cached_pages(BytePageSize::Size256), 1);

        // without a page size it is freed, not cached
        drop(st);
        for size in crate::size::PAGE_SIZES {
            assert_eq!(
                cache::cached_pages(size),
                usize::from(size == BytePageSize::Size256)
            );
        }
    }

    /// STATEMENT: Exact reservations use the requested capacity and classify storage as a page only at an exact page capacity.
    ///
    /// HOW:
    /// - Reserve 5,000 bytes after 100 bytes in unique and shared `Size4` pages and check capacity 5,100, `Unset`, data, and cache behavior.
    /// - Reserve exactly enough for `Size8` and verify that class is recognized with unchanged data.
    #[test]
    fn reserve_exact_pooled_page() {
        cache::reset();

        // a unique page is reallocated to the exact capacity
        let data = pattern(100);
        let mut st = StorageVec::sized(BytePageSize::Size4);
        assert_eq!(st.put_slice_partial(&data), data.len());
        st.reserve_exact(5000);
        assert_eq!(st.capacity(), 5100);
        assert_eq!(st.page_size(), BytePageSize::Unset);
        assert_eq!(st.as_ref(), &data[..]);
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 0);

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
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 1);

        // an exact page capacity is a page
        let mut st = StorageVec::sized(BytePageSize::Size4);
        assert_eq!(st.put_slice_partial(&data), data.len());
        st.reserve_exact(BytePageSize::Size8.capacity() - 100);
        assert_eq!(st.page_size(), BytePageSize::Size8);
        assert_eq!(st.as_ref(), &data[..]);
    }

    /// STATEMENT: Ordinary allocations remain unsized when grown unless their allocation exactly matches a page class.
    ///
    /// HOW:
    /// - Grow unique and shared ordinary storage and check `Unset` without page rounding.
    /// - Allocate exact `Size4` and `Size8` capacities, verify caching, and check a reused page starts empty and accepts new bytes.
    #[test]
    fn reserve_unsized_buffer_keeps_page_size() {
        cache::reset();

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
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 1);
        let st = StorageVec::from_slice(BytePageSize::Size8.capacity(), b"hello");
        assert_eq!(st.page_size(), BytePageSize::Size8);
        drop(st);
        assert_eq!(cache::cached_pages(BytePageSize::Size8), 1);

        // the cached page is reset
        let mut st = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(st.len(), 0);
        assert_eq!(st.remaining(), BytePageSize::Size8.capacity());
        assert_eq!(st.put_slice_partial(b"world"), 5);
        assert_eq!(st.as_ref(), b"world");
    }

    /// STATEMENT: Remaining capacity follows initialized data and view offsets and is restored when unique storage is reclaimed.
    ///
    /// HOW:
    /// - Write, advance, truncate, clear, and fill storage, checking length, capacity, remaining space, and fullness at each step.
    /// - Advance and freeze a mutable buffer, convert it back without moving, fill its spare space, and verify zero remains.
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

    /// STATEMENT: Truncating to zero reclaims consumed front capacity only for uniquely owned storage.
    ///
    /// HOW:
    /// - Advance a capacity-128 buffer and truncate to zero, checking full capacity returns.
    /// - Repeat while holding a split view and verify capacity retains the consumed offset.
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

    /// STATEMENT: Resizing can shorten data and then extend it with the requested fill byte.
    ///
    /// HOW:
    /// - Resize `hello world` to five bytes and then seven using `!`, checking `hello` and `hello!!`.
    #[test]
    fn resize_shrinks() {
        let mut b = BytesMut::copy_from_slice(b"hello world");
        b.resize(5, 0);
        assert_eq!(&b[..], b"hello");
        b.resize(7, b'!');
        assert_eq!(&b[..], b"hello!!");
    }

    /// STATEMENT: Reserving unique storage can reclaim consumed front space instead of increasing allocation capacity.
    ///
    /// HOW:
    /// - Consume 40 bytes before a ten-byte tail, reserve one more than current spare space, and check the tail, original capacity, and uniqueness.
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

    /// STATEMENT: Exact reservations preserve data for unique and shared buffers without amortized over-allocation.
    ///
    /// HOW:
    /// - Fill 64 bytes, reserve zero then 1,000, and check unchanged contents and tightly bounded capacity.
    /// - Keep a split view, reserve 2,000 for the tail, and verify tight capacity, unique new storage, and unchanged head data.
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

#[cfg(all(test, feature = "tracker"))]
mod tracker_tests {
    use super::*;
    use crate::*;

    const PAGE4: usize = 4 * 1024;
    const PAGE8: usize = 8 * 1024;

    fn snap(tracker: &BytesTracker) -> (usize, usize) {
        let s = tracker.snapshot();
        (s.live_bytes, s.live_allocations)
    }

    fn alloc_size(b: &BytesMut) -> usize {
        unsafe { Header::capacity(b.storage.0.as_ptr()) + METADATA_SIZE }
    }

    fn is_tracked(b: &BytesMut) -> bool {
        unsafe { !Header::tracker(b.storage.0.as_ptr()).is_null() }
    }

    /// STATEMENT: Tracked page allocations remain charged while live but lose attribution when returned to the cache.
    ///
    /// HOW:
    /// - Allocate and drop tracked `Size4`, check its charge clears, then reuse its pointer untracked and verify no tracker is attached.
    /// - Allocate tracked `Unset` and cached `Size4` pages, check `Size64` normalization and combined charges, then drop both to zero.
    #[test]
    fn page_charged_until_released() {
        cache::reset();
        let tracker = BytesTracker::new();

        let b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        assert_eq!(b.page_size(), BytePageSize::Size4);
        assert_eq!(snap(&tracker), (PAGE4, 1));
        let ptr = b.as_ptr();

        // a cached page is not attributed
        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 1);
        let b = BytesMut::with_page_size(BytePageSize::Size4);
        assert_eq!(b.as_ptr(), ptr);
        assert!(!is_tracked(&b));
        drop(b);

        // a cache hit is charged
        let b = BytesMut::with_page_size_in(BytePageSize::Unset, &tracker);
        assert_eq!(b.page_size(), BytePageSize::Size64);
        assert_eq!(snap(&tracker), (64 * 1024, 1));
        let b2 = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        assert_eq!(b2.as_ptr(), ptr);
        assert_eq!(snap(&tracker), (64 * 1024 + PAGE4, 2));
        drop((b, b2));
        assert_eq!(snap(&tracker), (0, 0));
        cache::reset();
    }

    /// STATEMENT: Split handles share one allocation charge and unique conversion retains that charge until final release.
    ///
    /// HOW:
    /// - Split a tracked `Size4` page into three handles, drop two, and check one full-page charge remains.
    /// - Convert the last handle to `BytesMut`, verify tracking persists, and drop it to clear the charge.
    #[test]
    fn handles_share_the_charge() {
        cache::reset();
        let tracker = BytesTracker::new();

        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 200]);
        let head = b.split_to(100);
        let view = b.split_to(50);
        assert_eq!(snap(&tracker), (PAGE4, 1));

        // the last handle releases the page
        drop(b);
        drop(view);
        assert_eq!(snap(&tracker), (PAGE4, 1));

        // a unique view converts without a copy and keeps the charge
        let b = BytesMut::from(head);
        assert!(is_tracked(&b));
        assert_eq!(snap(&tracker), (PAGE4, 1));
        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
        cache::reset();
    }

    /// STATEMENT: Copying shared tracked `Bytes` into mutable storage creates an untracked allocation.
    ///
    /// HOW:
    /// - Freeze and clone a tracked page, convert one view to `BytesMut`, and verify it is untracked while the original page stays charged.
    /// - Drop the remaining immutable view and check tracker totals return to zero before dropping the copy.
    #[test]
    fn shared_copy_from_bytes_is_untracked() {
        cache::reset();
        let tracker = BytesTracker::new();

        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 200]);
        let bytes = b.freeze();
        let other = bytes.clone();
        let b = BytesMut::from(bytes);
        assert!(!is_tracked(&b));
        assert_eq!(snap(&tracker), (PAGE4, 1));
        drop(other);
        assert_eq!(snap(&tracker), (0, 0));
        drop(b);
        cache::reset();
    }

    /// STATEMENT: Reallocating tracked storage adjusts charged bytes without adding another live allocation.
    ///
    /// HOW:
    /// - Grow a tracked page with two exact reservations, compare snapshots with actual allocation sizes, and verify original data remains.
    /// - Drop the grown buffer and check zero bytes and allocations remain charged.
    #[test]
    fn realloc_resizes_charge() {
        cache::reset();
        let tracker = BytesTracker::new();

        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 100]);
        b.reserve_exact(10_000);
        assert!(alloc_size(&b) > PAGE4);
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));

        b.extend_from_slice(&[2; 10_000]);
        b.reserve_exact(50_000);
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));
        assert_eq!(&b[..100], &[1; 100]);

        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: Page growth carries tracker attribution and shared old pages remain charged until their views drop.
    ///
    /// HOW:
    /// - Grow tracked `Size4` to `Size8`, checking its single new charge and cached old page.
    /// - Split a view and force replacement `Size8` storage, check two charges and a new pointer, then release handles and check charges fall to zero.
    #[test]
    fn move_to_page_carries_tracker() {
        cache::reset();
        let tracker = BytesTracker::new();

        // a unique page grows into a larger page, the old one is released
        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 5000]);
        assert_eq!(b.page_size(), BytePageSize::Size8);
        assert_eq!(snap(&tracker), (PAGE8, 1));
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 1);

        // a shared page stays charged until its last handle is dropped, the
        // data moves to a new page of the same size
        let head = b.split_to(1000);
        let ptr = b.as_ptr();
        b.extend_from_slice(&[2; 4000]);
        assert_eq!(b.page_size(), BytePageSize::Size8);
        assert_ne!(b.as_ptr(), ptr);
        assert_eq!(snap(&tracker), (2 * PAGE8, 2));
        drop(head);
        assert_eq!(snap(&tracker), (PAGE8, 1));

        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
        cache::reset();
    }

    /// STATEMENT: New ordinary allocations copied from shared tracked pages inherit their tracker.
    ///
    /// HOW:
    /// - Split tracked `Size4` buffers, then reserve exactly 10,000 bytes or reserve beyond all page classes.
    /// - Check `Unset` storage and combined old/new allocation charges, then release each old view and new buffer and verify totals decrease to zero.
    #[test]
    fn new_allocation_carries_tracker() {
        cache::reset();
        let tracker = BytesTracker::new();

        // an exact reservation of a shared buffer copies into a new buffer
        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 200]);
        let head = b.split_to(100);
        b.reserve_exact(10_000);
        assert_eq!(b.page_size(), BytePageSize::Unset);
        assert_eq!(alloc_size(&b), 10_100 + METADATA_SIZE);
        assert_eq!(snap(&tracker), (PAGE4 + alloc_size(&b), 2));
        drop(head);
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));
        drop(b);

        // above the largest page size
        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 200]);
        let head = b.split_to(100);
        b.reserve(1024 * 1024);
        assert_eq!(b.page_size(), BytePageSize::Unset);
        assert_eq!(snap(&tracker), (PAGE4 + alloc_size(&b), 2));
        drop(head);
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));
        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
        cache::reset();
    }

    /// STATEMENT: Trimming creates untracked copies or inline data and releases charges when original storage is no longer referenced.
    ///
    /// HOW:
    /// - Trim shared head and tail views from a tracked page and check the charge persists until both detach.
    /// - Trim a short frozen tracked page inline and verify its charge clears as well.
    #[test]
    fn trimdown_drops_tracker() {
        cache::reset();
        let tracker = BytesTracker::new();

        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 400]);
        let mut head = b.split_to(128);
        let mut tail = b.freeze();

        // the copies are untracked, shared or not
        head.trimdown();
        assert_eq!(snap(&tracker), (4096, 1));
        tail.trimdown();
        assert_eq!(snap(&tracker), (0, 0));

        // inline data is not attributed
        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 10]);
        let mut bytes = b.freeze();
        bytes.trimdown();
        assert_eq!(snap(&tracker), (0, 0));
        drop((head, tail, bytes));
        cache::reset();
    }

    /// STATEMENT: Cloning or explicitly copying tracked mutable data does not inherit allocation attribution.
    ///
    /// HOW:
    /// - Clone and copy a tracked 200-byte buffer, check both copies are untracked, and drop the source to verify zero tracker totals.
    #[test]
    fn untracked_copies() {
        let tracker = BytesTracker::new();
        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 200]);

        let copies = (b.clone(), BytesMut::copy_from_slice(&b[..]));
        assert!(!is_tracked(&copies.0));
        assert!(!is_tracked(&copies.1));
        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: A tracked allocation keeps tracker state alive through growth after public tracker handles are dropped.
    ///
    /// HOW:
    /// - Drop a page's tracker handle, grow the page to 5,000 bytes, split ten bytes, and release both allocation views safely.
    #[test]
    fn tracker_outlives_handles() {
        let tracker = BytesTracker::new();
        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        drop(tracker);
        // grows the page charged to a tracker without handles
        b.extend_from_slice(&[1; 5000]);
        let head = b.split_to(10);
        drop(b);
        // the last allocation frees the tracker state
        drop(head);
    }

    /// STATEMENT: Releasing shared page handles on other threads clears the allocation's tracker charge.
    ///
    /// HOW:
    /// - Split a tracked 200-byte page, drop head and tail in separate threads, join them, and verify zero live totals.
    #[test]
    fn release_on_other_thread() {
        let tracker = BytesTracker::new();
        let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
        b.extend_from_slice(&[1; 200]);
        let head = b.split_to(100);

        let t1 = std::thread::spawn(move || drop(head));
        let t2 = std::thread::spawn(move || drop(b));
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: Attaching a tracker adopts an existing allocation and keeps its charge through growth and freezing.
    ///
    /// HOW:
    /// - Attach a tracker to an untracked buffer and check tracker identity and allocation-sized charge.
    /// - Grow and freeze it, verify one correctly sized live allocation, then drop it and check zero totals.
    #[test]
    fn set_tracker_adopts_allocation() {
        let tracker = BytesTracker::new();
        let mut b = BytesMut::with_capacity(100);
        b.extend_from_slice(&[1; 50]);
        assert!(b.tracker().is_none());

        b.set_tracker(&tracker);
        assert!(b.tracker().unwrap().ptr_eq(&tracker));
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));

        // growth and freeze keep the charge
        b.extend_from_slice(&[1; 1000]);
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));
        let frozen = b.freeze();
        assert_eq!(snap(&tracker).1, 1);
        drop(frozen);
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: Reassigning trackers moves a shared or unique allocation's charge exactly once.
    ///
    /// HOW:
    /// - Adopt a shared allocation, assign the same tracker and its clone, and verify its charge remains unchanged.
    /// - Move attribution to another tracker, drop the split view, move it back, and check old/new totals and final release.
    #[test]
    fn set_tracker_reattributes() {
        let tracker = BytesTracker::new();
        let other = BytesTracker::new();
        let mut b = BytesMut::with_capacity(200);
        b.extend_from_slice(&[1; 150]);
        let size = alloc_size(&b);

        // a shared untracked allocation is adopted
        let head = b.split_to(64);
        b.set_tracker(&tracker);
        assert_eq!(snap(&tracker), (size, 1));

        // the same tracker is a no-op
        b.set_tracker(&tracker);
        b.set_tracker(&tracker.clone());
        assert_eq!(snap(&tracker), (size, 1));

        // the charge moves while shared and while unique
        b.set_tracker(&other);
        assert!(b.tracker().unwrap().ptr_eq(&other));
        assert_eq!(snap(&tracker), (0, 0));
        assert_eq!(snap(&other), (size, 1));
        drop(head);
        b.set_tracker(&tracker);
        assert_eq!(snap(&tracker), (size, 1));
        assert_eq!(snap(&other), (0, 0));

        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: Reattribution safely releases tracker state held only by an allocation and retains the replacement tracker.
    ///
    /// HOW:
    /// - Attach a temporary tracker, replace it, drop the replacement's public handle, and recover a clone through the buffer.
    /// - Check the live allocation charge and verify dropping the buffer clears it.
    #[test]
    fn set_tracker_releases_last_tracker_handle() {
        let mut b = BytesMut::with_capacity(200);
        b.set_tracker(&BytesTracker::new());

        // the old tracker is freed by the move, the new one stays alive
        let other = BytesTracker::new();
        b.set_tracker(&other);
        drop(other);
        let tracker = b.tracker().unwrap().clone();
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));
        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: Tracker reassignment remains correct while another thread detaches a shared view.
    ///
    /// HOW:
    /// - Split a tracked buffer, trim the head on another thread, and reassign the tail to a different tracker concurrently.
    /// - Join, check head length and old/new charges, then drop the tail and verify the new charge clears.
    #[test]
    fn set_tracker_while_shared_across_threads() {
        let tracker = BytesTracker::new();
        let other = BytesTracker::new();
        let mut b = BytesMut::with_capacity(512);
        b.extend_from_slice(&[1; 400]);
        b.set_tracker(&tracker);
        let size = alloc_size(&b);

        let mut head = b.split_to(128);
        let handle = std::thread::spawn(move || {
            // shared, the copy is not attributed
            head.trimdown();
            head
        });
        b.set_tracker(&other);
        let head = handle.join().unwrap();
        assert_eq!(head.len(), 128);
        assert_eq!(snap(&tracker).1, 0);
        assert_eq!(snap(&other), (size, 1));
        drop(b);
        assert_eq!(snap(&other), (0, 0));
    }

    /// STATEMENT: Adopting a cached-size page charges it only while live and does not leave tracker state in the cache.
    ///
    /// HOW:
    /// - Attach a tracker to untracked `Size4`, check its page charge, drop it, and verify zero totals and one cached page.
    /// - Reuse the page without a tracker and check `tracker()` returns `None`.
    #[test]
    fn set_tracker_on_page() {
        cache::reset();
        let tracker = BytesTracker::new();
        let mut b = BytesMut::with_page_size(BytePageSize::Size4);
        b.set_tracker(&tracker);
        assert_eq!(snap(&tracker), (PAGE4, 1));

        // uncharged when returned to the cache
        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 1);
        let b = BytesMut::with_page_size(BytePageSize::Size4);
        assert!(b.tracker().is_none());
    }

    /// STATEMENT: Concurrent final releases of many shared pages remove every allocation charge exactly once.
    ///
    /// HOW:
    /// - Allocate tracked pages in four groups, split heads and tails, and check the full aggregate charge.
    /// - Synchronize eight releasing threads with a barrier, join them, and verify zero live bytes and allocations.
    #[test]
    fn concurrent_releases() {
        const THREADS: usize = 4;
        const BUFS: usize = if cfg!(miri) { 4 } else { 64 };

        let tracker = BytesTracker::new();
        let mut heads = Vec::new();
        let mut tails = Vec::new();
        for _ in 0..THREADS {
            let (mut h, mut t) = (Vec::new(), Vec::new());
            for _ in 0..BUFS {
                let mut b = BytesMut::with_page_size_in(BytePageSize::Size4, &tracker);
                b.extend_from_slice(&[1; 200]);
                h.push(b.split_to(100));
                t.push(b);
            }
            heads.push(h);
            tails.push(t);
        }
        assert_eq!(snap(&tracker), (THREADS * BUFS * PAGE4, THREADS * BUFS));

        // views of each page are released concurrently on different threads
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS * 2));
        let mut threads = Vec::new();
        for (h, t) in heads.into_iter().zip(tails) {
            let b1 = barrier.clone();
            let b2 = barrier.clone();
            threads.push(std::thread::spawn(move || {
                b1.wait();
                drop(h);
            }));
            threads.push(std::thread::spawn(move || {
                b2.wait();
                drop(t);
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: Evicting already-unattributed cached pages does not alter live tracker charges or access retired tracker state.
    ///
    /// HOW:
    /// - Cache pages from two trackers, keep a tracked `Size8` live, and drop the tracker with no remaining allocations.
    /// - Lower the cache limit and reset the cache, check only `Size8` remains charged, then drop it to clear totals.
    #[test]
    fn cache_eviction_does_not_uncharge() {
        cache::reset();
        let freed = BytesTracker::new();
        let tracker = BytesTracker::new();
        let pages = [
            BytesMut::with_page_size_in(BytePageSize::Size4, &freed),
            BytesMut::with_page_size_in(BytePageSize::Size4, &tracker),
            BytesMut::with_page_size_in(BytePageSize::Size4, &tracker),
        ];
        let live = BytesMut::with_page_size_in(BytePageSize::Size8, &tracker);
        drop(pages);
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 3);
        assert_eq!(snap(&tracker), (PAGE8, 1));
        // no attributed allocation left, the state is freed
        drop(freed);

        // evicted and dropped cached pages do not touch the trackers
        crate::set_page_cache_size(BytePageSize::Size4, 1);
        assert_eq!(cache::cached_pages(BytePageSize::Size4), 1);
        cache::reset();
        assert_eq!(snap(&tracker), (PAGE8, 1));
        drop(live);
        assert_eq!(snap(&tracker), (0, 0));
    }

    /// STATEMENT: A cached page can be reassigned to a new tracker without retaining the previous one.
    ///
    /// HOW:
    /// - Drop a tracked `Size4` page and its original tracker after checking the charge clears.
    /// - Reuse its pointer with another tracker, check identity and charge, and drop it to return totals to zero.
    #[test]
    fn cache_reuse_by_other_tracker() {
        cache::reset();
        let first = BytesTracker::new();
        let second = BytesTracker::new();

        let page = BytesMut::with_page_size_in(BytePageSize::Size4, &first);
        let ptr = page.as_ptr();
        drop(page);
        assert_eq!(snap(&first), (0, 0));
        // the cached page does not keep the first tracker alive
        drop(first);

        let page = BytesMut::with_page_size_in(BytePageSize::Size4, &second);
        assert_eq!(page.as_ptr(), ptr);
        assert!(page.tracker().unwrap().ptr_eq(&second));
        assert_eq!(snap(&second), (PAGE4, 1));
        drop(page);
        assert_eq!(snap(&second), (0, 0));
    }

    /// STATEMENT: An allocation-owned tracker can be cloned through the buffer after its original public handle disappears.
    ///
    /// HOW:
    /// - Attach a temporary tracker to a buffer, clone its borrowed tracker, and check the allocation-sized charge.
    /// - Drop the buffer and verify the recovered tracker reports zero totals.
    #[test]
    fn borrowed_tracker_without_handles() {
        let mut b = BytesMut::with_capacity(100);
        b.set_tracker(&BytesTracker::new());

        // the charge keeps the tracker alive
        let tracker = b.tracker().unwrap().clone();
        assert_eq!(snap(&tracker), (alloc_size(&b), 1));
        drop(b);
        assert_eq!(snap(&tracker), (0, 0));
    }
}
