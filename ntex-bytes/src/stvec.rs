use crate::alloc::alloc::{self, Layout, LayoutError};

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{self, AtomicU32};
use std::{cell::Cell, mem, ptr, ptr::NonNull, slice};

use crate::{BytePageSize, storage::Storage};

#[derive(Debug)]
/// Thread-safe reference-counted container for the shared storage.
pub(crate) struct SharedVec {
    pub(crate) offset: u32,
    pub(crate) len: u32,
    pub(crate) capacity: u32,
    pub(crate) remaining: u32,
    pub(crate) ref_count: AtomicU32,
    pub(crate) size: BytePageSize,
}

#[derive(Debug)]
pub(crate) struct StorageVec(pub(crate) NonNull<SharedVec>);

// Buffer storage strategy flags.
const KIND_VEC: usize = 0b01;
const KIND_OFFSET_BITS: usize = 2;

pub const METADATA_SIZE: usize = mem::size_of::<SharedVec>();
const METADATA_SIZE_U32: u32 = METADATA_SIZE as u32;

// Inline buffer capacity. This is the size of `Storage` minus 1 byte for the
// metadata.
#[cfg(target_pointer_width = "64")]
pub(crate) const INLINE_CAP: usize = 3 * 8 - 1;
#[cfg(target_pointer_width = "32")]
pub(crate) const INLINE_CAP: usize = 3 * 4 - 1;

impl StorageVec {
    /// Create new empty storage with specified capacity
    pub(crate) fn with_capacity(capacity: usize) -> StorageVec {
        StorageVec(SharedVec::create(BytePageSize::Unset, capacity, &[]))
    }

    /// Create new empty storage with specified size category
    pub(crate) fn sized(size: BytePageSize) -> StorageVec {
        CACHE.with(|c| {
            let mut cst = c.take().unwrap();
            let item = cst.cache[size as usize].pop();
            c.set(Some(cst));

            if let Some(mut item) = item {
                unsafe {
                    item.as_inner().size = size;
                }
                item
            } else {
                StorageVec(SharedVec::create(size, size.capacity(), &[]))
            }
        })
    }

    /// Create new storage with capacity and copy slice
    ///
    /// Caller must guarantee cap is larger or equal to src length
    pub(crate) fn from_slice(capacity: usize, src: &[u8]) -> StorageVec {
        StorageVec(SharedVec::create(BytePageSize::Unset, capacity, src))
    }

    pub(crate) fn unsize(&mut self) {
        unsafe { (*self.0.as_ptr()).size = BytePageSize::Unset }
    }

    #[allow(dead_code)]
    /// Returns the page size type
    pub(crate) fn page_size(&self) -> BytePageSize {
        unsafe { (*self.0.as_ptr()).size }
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
        unsafe { (*self.0.as_ptr()).capacity as usize }
    }

    pub(crate) fn remaining(&self) -> usize {
        unsafe { (*self.0.as_ptr()).remaining as usize }
    }

    pub(crate) fn is_full(&self) -> bool {
        unsafe { (*self.0.as_ptr()).remaining == 0 }
    }

    pub(crate) fn is_unique(&mut self) -> bool {
        unsafe { (*self.0.as_ptr()).ref_count.load(Relaxed) == 1 }
    }

    /// The caller must guarantee that `StorageVec` is not being used for
    /// memory modification.
    pub(crate) unsafe fn clone(&self) -> StorageVec {
        let ref_cnt = self.0.as_ref().ref_count.fetch_add(1, Relaxed);
        if ref_cnt == u32::MAX {
            abort();
        }
        StorageVec(self.0)
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
                    offset: (offset << KIND_OFFSET_BITS) ^ KIND_VEC,
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
                let ref_cnt = inner.ref_count.fetch_add(1, Relaxed);
                if ref_cnt == u32::MAX {
                    abort();
                }

                let offset = inner.offset as usize;
                Storage {
                    ptr: (self.0.as_ptr().cast::<u8>()).add(offset),
                    len: at,
                    offset: (offset << KIND_OFFSET_BITS) ^ KIND_VEC,
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
                    let cap = (inner.offset as usize) + inner.capacity as usize;
                    inner.len = 0;
                    inner.offset = METADATA_SIZE_U32;
                    inner.capacity = (cap - METADATA_SIZE) as u32;
                    inner.remaining = inner.capacity;
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
        *self = StorageVec(SharedVec::create(
            BytePageSize::Unset,
            capacity,
            self.as_ref(),
        ));
    }

    #[inline]
    pub(crate) fn reserve(&mut self, additional: usize) {
        if additional <= self.remaining() {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.reserve_inner(additional);
    }

    fn reserve_inner(&mut self, additional: usize) {
        unsafe {
            let inner = self.as_inner();
            let len = inner.len as usize;

            // Reserving involves abandoning the currently shared buffer and
            // allocating a new vector with the requested capacity.
            let new_cap = len + additional;

            if inner.is_unique() {
                let capacity = (inner.offset as usize) + (inner.capacity as usize);

                // try to reclaim the buffer. This is possible if the current
                // handle is the only outstanding handle pointing to the buffer.
                if capacity >= (new_cap + METADATA_SIZE) {
                    let offset = inner.offset;
                    inner.offset = METADATA_SIZE_U32;
                    inner.remaining = (capacity - len - METADATA_SIZE) as u32;
                    inner.capacity = inner.len + inner.remaining;

                    // The capacity is sufficient, reclaim the buffer
                    if len != 0 {
                        let ptr = self.0.as_ptr().cast::<u8>();
                        ptr::copy(ptr.add(offset as usize), ptr.add(METADATA_SIZE), len);
                    }
                    return;
                }
            }
            // Create a new storage
            *self = StorageVec(SharedVec::create(
                BytePageSize::Unset,
                new_cap,
                self.as_ref(),
            ));
        }
    }

    #[inline]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        let inner = self.0.as_mut();
        assert!(len as u32 <= inner.capacity);

        inner.len = len as u32;
        inner.remaining = inner.capacity - (len as u32);
    }

    pub(crate) unsafe fn set_start(&mut self, start: u32) {
        if start != 0 {
            let inner = self.as_inner();

            assert!(
                start <= inner.capacity,
                "Cannot set start position offset:{} len:{} cap:{} remaining:{} new-len:{start}",
                inner.offset,
                inner.len,
                inner.capacity,
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
            inner.remaining = inner.capacity - inner.len - start;
            inner.capacity = inner.remaining + inner.len;
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

// TODO: Drop *mut SharedVec on thread local destroy
thread_local! {
    static CACHE: Cell<Option<Box<Cache>>> = Cell::new(Some(Box::default()));
}

pub(crate) fn set_pages_cache(size: usize) {
    CACHE.with(|c| {
        let mut cst = c.take().unwrap();
        cst.size = size;
        c.set(Some(cst));
    });
}

struct Cache {
    size: usize,
    cache: [Vec<StorageVec>; 7],
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            size: 128,
            cache: Default::default(),
        }
    }
}

impl SharedVec {
    pub(crate) fn create(size: BytePageSize, cap: usize, src: &[u8]) -> NonNull<SharedVec> {
        let ptr = Self::alloc_with_capacity(size, cap, src.len() as u32);

        // copy slice
        unsafe {
            let dst = ptr.add(METADATA_SIZE);
            let sl = slice::from_raw_parts_mut(dst, src.len());
            sl.copy_from_slice(src);
            #[allow(clippy::cast_ptr_alignment)]
            NonNull::new_unchecked(ptr.cast::<SharedVec>())
        }
    }

    fn alloc_with_capacity(size: BytePageSize, cap: usize, len: u32) -> *mut u8 {
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
                    size,
                    remaining: capacity - len,
                    offset: METADATA_SIZE_U32,
                    ref_count: AtomicU32::new(1),
                },
            );
            ptr
        }
    }

    fn is_unique(&self) -> bool {
        // This is same as Shared::is_unique() but for KIND_VEC
        self.ref_count.load(Acquire) == 1
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity as usize
    }
}

pub(crate) fn release_shared_vec(ptr: *mut SharedVec) {
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

        let cap = (*ptr).offset + (*ptr).capacity;

        // Try to put to cache
        let size = (*ptr).size;
        if size != BytePageSize::Unset {
            let cached = CACHE.with(|c| {
                let mut cst = c.take().unwrap();
                let res = if cst.cache[size as usize].len() < cst.size {
                    let capacity = cap - METADATA_SIZE_U32;
                    (*ptr).len = 0;
                    (*ptr).offset = METADATA_SIZE_U32;
                    (*ptr).capacity = capacity;
                    (*ptr).remaining = capacity;
                    (*ptr).ref_count = AtomicU32::new(1);
                    (*ptr).size = BytePageSize::Unset;
                    cst.cache[size as usize].push(StorageVec(NonNull::new_unchecked(ptr)));
                    true
                } else {
                    false
                };
                c.set(Some(cst));
                res
            });
            if cached {
                return;
            }
        }

        // Drop the data
        ptr::drop_in_place(ptr);
        let layout = shared_vec_layout(cap as usize - METADATA_SIZE).unwrap();
        alloc::dealloc(ptr.cast(), layout);
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

#[inline(never)]
#[cold]
pub(crate) fn abort() {
    let _a = Abort;
    panic!();
}

#[cfg(test)]
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
}
