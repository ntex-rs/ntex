use crate::alloc::alloc::{self, Layout, LayoutError};

use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{self, AtomicUsize};
use std::{cmp, mem, ptr, ptr::NonNull, slice};

use crate::{BytesMut, info::Info, info::Kind};

// Both `Bytes` and `BytesMut` are backed by `Storage` and functions are delegated
// to `Storage` functions. The `Bytes` and `BytesMut` shims ensure that functions
// that mutate the underlying buffer are only performed when the data range
// being mutated is only available via a single `BytesMut` handle.
//
// # Data storage modes
//
// The goal of `bytes` is to be as efficient as possible across a wide range of
// potential usage patterns. As such, `bytes` needs to be able to handle buffers
// that are never shared, shared on a single thread, and shared across many
// threads. `bytes` also needs to handle both tiny buffers as well as very large
// buffers. For example, [Cassandra](http://cassandra.apache.org) values have
// been known to be in the hundreds of megabyte, and HTTP header values can be a
// few characters in size.
//
// To achieve high performance in these various situations, `Bytes` and
// `BytesMut` use different strategies for storing the buffer depending on the
// usage pattern.
//
// ## Delayed `Arc` allocation
//
// When a `Bytes` or `BytesMut` is first created, there is only one outstanding
// handle referencing the buffer. Since sharing is not yet required, an `Arc`* is
// not used and the buffer is backed by a `Vec<u8>` directly. Using an
// `Arc<Vec<u8>>` requires two allocations, so if the buffer ends up never being
// shared, that allocation is avoided.
//
// When sharing does become necessary (`clone`, `split_to`, `split_off`), that
// is when the buffer is promoted to being shareable. The `Vec<u8>` is moved
// into an `Arc` and both the original handle and the new handle use the same
// buffer via the `Arc`.
//
// * `Arc` is being used to signify an atomically reference counted cell. We
// don't use the `Arc` implementation provided by `std` and instead use our own.
// This ends up simplifying a number of the `unsafe` code snippets.
//
// ## Inlining small buffers
//
// The `Bytes` / `BytesMut` structs require 4 pointer sized fields. On 64 bit
// systems, this ends up being 32 bytes, which is actually a lot of storage for
// cases where `Bytes` is being used to represent small byte strings, such as
// HTTP header names and values.
//
// To avoid any allocation at all in these cases, `Bytes` will use the struct
// itself for storing the buffer, reserving 1 byte for meta data. This means
// that, on 64 bit systems, 31 byte buffers require no allocation at all.
//
// The byte used for metadata stores a 2 bits flag used to indicate that the
// buffer is stored inline as well as 6 bits for tracking the buffer length (the
// return value of `Bytes::len`).
//
// ## Static buffers
//
// `Bytes` can also represent a static buffer, which is created with
// `Bytes::from_static`. No copying or allocations are required for tracking
// static buffers. The pointer to the `&'static [u8]`, the length, and a flag
// tracking that the `Bytes` instance represents a static buffer is stored in
// the `Bytes` struct.
//
// # Struct layout
//
// Both `Bytes` and `BytesMut` are wrappers around `Storage`, which provides the
// data fields as well as all of the function implementations.
//
// The `Storage` struct is carefully laid out in order to support the
// functionality described above as well as being as small as possible. Size is
// important as growing the size of the `Bytes` struct from 32 bytes to 40 bytes
// added as much as 15% overhead in benchmarks using `Bytes` in an HTTP header
// map structure.
//
// The `Storage` struct contains the following fields:
//
// * `ptr: *mut u8`
// * `len: usize`
// * `cap: usize`
// * `arc: *mut Shared`
//
// ## `ptr: *mut u8`
//
// A pointer to start of the handle's buffer view. When backed by a `Vec<u8>`,
// this is always the `Vec`'s pointer. When backed by an `Arc<Vec<u8>>`, `ptr`
// may have been shifted to point somewhere inside the buffer.
//
// When in "inlined" mode, `ptr` is used as part of the inlined buffer.
//
// ## `len: usize`
//
// The length of the handle's buffer view. When backed by a `Vec<u8>`, this is
// always the `Vec`'s length. The slice represented by `ptr` and `len` should
// (ideally) always be initialized memory.
//
// When in "inlined" mode, `len` is used as part of the inlined buffer.
//
// ## `cap: usize`
//
// The capacity of the handle's buffer view. When backed by a `Vec<u8>`, this is
// always the `Vec`'s capacity. The slice represented by `ptr+len` and `cap-len`
// may or may not be initialized memory.
//
// When in "inlined" mode, `cap` is used as part of the inlined buffer.
//
// ## `arc: *mut Shared`
//
// When `Storage` is in allocated mode (backed by Vec<u8> or Arc<Vec<u8>>), this
// will be the pointer to the `Arc` structure tracking the ref count for the
// underlying buffer. When the pointer is null, then the `Arc` has not been
// allocated yet and `self` is the only outstanding handle for the underlying
// buffer.
//
// The lower two bits of `arc` are used to track the storage mode of `Storage`.
// `0b01` indicates inline storage, `0b10` indicates static storage, and `0b11`
// indicates vector storage, not yet promoted to Arc.  Since pointers to
// allocated structures are aligned, the lower two bits of a pointer will always
// be 0. This allows disambiguating between a pointer and the two flags.
//
// When in "inlined" mode, the least significant byte of `arc` is also used to
// store the length of the buffer view (vs. the capacity, which is a constant).
//
// The rest of `arc`'s bytes are used as part of the inline buffer, which means
// that those bytes need to be located next to the `ptr`, `len`, and `cap`
// fields, which make up the rest of the inline buffer. This requires special
// casing the layout of `Storage` depending on if the target platform is big or
// little endian.
//
// On little endian platforms, the `arc` field must be the first field in the
// struct. On big endian platforms, the `arc` field must be the last field in
// the struct. Since a deterministic struct layout is required, `Storage` is
// annotated with `#[repr(C)]`.
//
// # Thread safety
//
// `Bytes::clone()` returns a new `Bytes` handle with no copying. This is done
// by bumping the buffer ref count and returning a new struct pointing to the
// same buffer. However, the `Arc` structure is lazily allocated. This means
// that if `Bytes` is stored itself in an `Arc` (`Arc<Bytes>`), the `clone`
// function can be called concurrently from multiple threads. This is why an
// `AtomicPtr` is used for the `arc` field vs. a `*const`.
//
// Care is taken to ensure that the need for synchronization is minimized. Most
// operations do not require any synchronization.
//
#[cfg(target_endian = "little")]
#[repr(C)]
pub(crate) struct Storage {
    // WARNING: Do not access the fields directly unless you know what you are
    // doing. Instead, use the fns. See implementation comment above.
    arc: NonNull<Shared>,
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

#[cfg(target_endian = "big")]
#[repr(C)]
pub(crate) struct Storage {
    // WARNING: Do not access the fields directly unless you know what you are
    // doing. Instead, use the fns. See implementation comment above.
    ptr: *mut u8,
    len: usize,
    cap: usize,
    arc: NonNull<Shared>,
}

// Thread-safe reference-counted container for the shared storage. This mostly
// the same as `std::sync::Arc` but without the weak counter. The ref counting
// fns are based on the ones found in `std`.
//
// The main reason to use `Shared` instead of `std::sync::Arc` is that it ends
// up making the overall code simpler and easier to reason about. This is due to
// some of the logic around setting `Storage::arc` and other ways the `arc` field
// is used. Using `Arc` ended up requiring a number of funky transmutes and
// other shenanigans to make it work.
struct Shared {
    vec: Vec<u8>,
    ref_count: AtomicUsize,
}

#[repr(C)]
struct SharedVec {
    cap: usize,
    len: u32,
    offset: u32,
    ref_count: AtomicUsize,
    data: [u8; 0],
}

pub(crate) struct StorageVec(NonNull<SharedVec>);

// Buffer storage strategy flags.
const KIND_ARC: usize = 0b00;
const KIND_INLINE: usize = 0b01;
const KIND_STATIC: usize = 0b10;
const KIND_VEC: usize = 0b11;
const KIND_MASK: usize = 0b11;
const KIND_UNMASK: usize = !KIND_MASK;

const SHARED_VEC_SIZE: usize = mem::size_of::<SharedVec>();

// Bit op constants for extracting the inline length value from the `arc` field.
const INLINE_LEN_MASK: usize = 0b1111_1100;
const INLINE_LEN_OFFSET: usize = 2;

// Byte offset from the start of `Storage` to where the inline buffer data
// starts. On little endian platforms, the first byte of the struct is the
// storage flag, so the data is shifted by a byte. On big endian systems, the
// data starts at the beginning of the struct.
#[cfg(target_endian = "little")]
const INLINE_DATA_OFFSET: isize = 2;
#[cfg(target_endian = "big")]
const INLINE_DATA_OFFSET: isize = 0;

// Inline buffer capacity. This is the size of `Storage` minus 1 byte for the
// metadata.
#[cfg(target_pointer_width = "64")]
pub(crate) const INLINE_CAP: usize = 4 * 8 - 2;
#[cfg(target_pointer_width = "32")]
pub(crate) const INLINE_CAP: usize = 4 * 4 - 2;

// Inline storage
const PTR_INLINE: NonNull<Shared> =
    unsafe { NonNull::new_unchecked(KIND_INLINE as *mut Shared) };
// Inline storage
const PTR_STATIC: NonNull<Shared> =
    unsafe { NonNull::new_unchecked(KIND_STATIC as *mut Shared) };

/*
 *
 * ===== Storage =====
 *
 */

impl Storage {
    #[inline]
    pub(crate) const fn empty() -> Storage {
        Storage {
            arc: PTR_INLINE,
            ptr: ptr::null_mut::<u8>(),
            len: 0,
            cap: 0,
        }
    }

    #[inline]
    pub(crate) fn empty_inline() -> Storage {
        Storage::from_slice_with_capacity(INLINE_CAP, &[])
    }

    #[inline]
    pub(crate) fn with_capacity(capacity: usize) -> Storage {
        Self::from_shared_vec(SharedVec::create_with_capacity(capacity), capacity, 0)
    }

    #[inline]
    pub(crate) const fn from_static(bytes: &'static [u8]) -> Storage {
        let ptr = bytes.as_ptr() as *mut u8;

        Storage {
            // `arc` won't ever store a pointer. Instead, use it to
            // track the fact that the `Bytes` handle is backed by a
            // static buffer.
            arc: PTR_STATIC,
            ptr,
            len: bytes.len(),
            cap: bytes.len(),
        }
    }

    #[inline]
    pub(crate) fn from_vec(mut vec: Vec<u8>) -> Storage {
        let len = vec.len();
        let cap = vec.capacity();
        let ptr = vec.as_mut_ptr();

        // Store data in arc
        let shared = Box::into_raw(Box::new(Shared {
            vec,
            ref_count: AtomicUsize::new(1),
        }));

        // The pointer should be aligned, so this assert should always succeed.
        debug_assert!(0 == (shared as usize & KIND_MASK));

        // Create new arc, so atomic operations can be avoided.
        Storage {
            ptr,
            len,
            cap,
            arc: unsafe { NonNull::new_unchecked(shared) },
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
        Self::from_shared_vec(SharedVec::create(cap, src), cap, src.len())
    }

    #[inline]
    fn from_shared_vec(ptr: NonNull<SharedVec>, cap: usize, len: usize) -> Storage {
        unsafe {
            let arc = NonNull::new_unchecked(
                ptr.as_ptr().map_addr(|addr| addr ^ KIND_VEC) as *mut Shared
            );

            Storage {
                cap,
                len,
                arc,
                ptr: ptr.as_ptr().add(1) as *mut u8,
            }
        }
    }

    #[inline]
    unsafe fn from_ptr_inline(src: *const u8, len: usize) -> Storage {
        let mut st = Storage {
            arc: PTR_INLINE,
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
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
            if self.is_inline() {
                slice::from_raw_parts(self.inline_ptr_ro(), self.inline_len())
            } else {
                slice::from_raw_parts(self.ptr, self.len)
            }
        }
    }

    /// Return a mutable slice for the handle's view into the shared buffer
    #[inline]
    pub(crate) fn as_mut(&mut self) -> &mut [u8] {
        debug_assert!(self.kind() != KIND_STATIC);

        unsafe {
            if self.is_inline() {
                slice::from_raw_parts_mut(self.inline_ptr(), self.inline_len())
            } else {
                slice::from_raw_parts_mut(self.ptr, self.len)
            }
        }
    }

    /// Return a mutable slice for the handle's view into the shared buffer
    /// including potentially uninitialized bytes.
    #[inline]
    pub(crate) unsafe fn as_raw(&mut self) -> &mut [u8] {
        debug_assert!(self.kind() != KIND_STATIC);

        if self.is_inline() {
            slice::from_raw_parts_mut(self.inline_ptr(), INLINE_CAP)
        } else {
            slice::from_raw_parts_mut(self.ptr, self.cap)
        }
    }

    /// Return a raw pointer to data
    #[inline]
    unsafe fn as_ptr(&mut self) -> *mut u8 {
        if self.is_inline() {
            self.inline_ptr()
        } else {
            self.ptr
        }
    }

    /// Insert a byte into the next slot and advance the len by 1.
    #[inline]
    pub(crate) fn put_u8(&mut self, n: u8) {
        if self.is_inline() {
            let len = self.inline_len();
            unsafe {
                *self.inline_ptr().add(len) = n;
            }
            self.set_inline_len(len + 1);
        } else {
            unsafe {
                *self.ptr.add(self.len) = n;
            }
            self.len += 1;
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        if self.is_inline() {
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
        // This is undefind behavior due to a data race, but experimental
        // evidence shows that it works in practice (discussion:
        // https://internals.rust-lang.org/t/bit-wise-reasoning-for-atomic-accesses/8853).
        (self.arc.as_ptr() as usize & INLINE_LEN_MASK) >> INLINE_LEN_OFFSET
    }

    /// Set the length of the inline buffer. This is done by writing to the
    /// least significant byte of the `arc` field.
    #[inline]
    fn set_inline_len(&mut self, len: usize) {
        debug_assert!(len <= INLINE_CAP);
        self.arc = unsafe {
            NonNull::new_unchecked(
                self.arc
                    .as_ptr()
                    .map_addr(|addr| addr & !INLINE_LEN_MASK | (len << INLINE_LEN_OFFSET)),
            )
        };
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        if self.is_inline() { INLINE_CAP } else { self.cap }
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
            self.truncate(new_len, false);
        }
    }

    #[inline]
    pub(crate) fn freeze(self) -> Storage {
        if self.len() > INLINE_CAP || self.is_inline() {
            self
        } else {
            let slice = self.as_ref();
            unsafe { Storage::from_ptr_inline(slice.as_ptr(), slice.len()) }
        }
    }

    /// slice.
    #[inline]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        if self.is_inline() {
            assert!(len <= INLINE_CAP);
            self.set_inline_len(len);
        } else {
            assert!(len <= self.cap);
            self.len = len;
        }
    }

    pub(crate) unsafe fn set_start(&mut self, start: usize) {
        // Setting the start to 0 is a no-op, so return early if this is the
        // case.
        if start == 0 {
            return;
        }

        let kind = self.kind();

        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Storage` struct fields will be gibberish.
        if kind == KIND_INLINE {
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
        } else {
            assert!(start <= self.cap);

            // Updating the start of the view is setting `ptr` to point to the
            // new start and updating the `len` field to reflect the new length
            // of the view.
            self.ptr = self.ptr.add(start);

            if self.len >= start {
                self.len -= start;
            } else {
                self.len = 0;
            }

            self.cap -= start;
        }
    }

    pub(crate) unsafe fn set_end(&mut self, end: usize) {
        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Storage` struct fields will be gibberish.
        if self.is_inline() {
            assert!(end <= INLINE_CAP);
            let new_len = cmp::min(self.inline_len(), end);
            self.set_inline_len(new_len);
        } else {
            assert!(end <= self.cap);

            self.cap = end;
            self.len = cmp::min(self.len, end);
        }
    }

    /// Checks if it is safe to mutate the memory
    pub(crate) fn is_mut_safe(&self) -> bool {
        let kind = self.kind();

        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Storage` struct fields will be gibberish.
        if kind == KIND_INLINE {
            // Inlined buffers can always be mutated as the data is never shared
            // across handles.
            true
        } else if kind == KIND_STATIC {
            false
        } else if kind == KIND_VEC {
            // Otherwise, the underlying buffer is potentially shared with other
            // handles, so the ref_count needs to be checked.
            unsafe { (*self.shared_vec()).is_unique() }
        } else {
            // Otherwise, the underlying buffer is potentially shared with other
            // handles, so the ref_count needs to be checked.
            unsafe { (*self.arc.as_ptr()).is_unique() }
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
        // Additionally, if kind is STATIC, then Arc is *never* changed, making
        // it safe and faster to check for it now before an atomic acquire.

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
            self.shallow_clone_sync()
        }
    }

    unsafe fn shallow_clone_sync(&self) -> Storage {
        // The function requires `&self`, this means that `shallow_clone`
        // could be called concurrently.
        //
        // The first step is to load the value of `arc`. This will determine
        // how to proceed. The `Acquire` ordering synchronizes with the
        // `compare_and_swap` that comes later in this function. The goal is
        // to ensure that if `arc` is currently set to point to a `Shared`,
        // that the current thread acquires the associated memory.
        let arc: *mut Shared = self.arc.as_ptr();
        let kind = self.kind();

        if kind == KIND_ARC {
            let old_size = (*arc).ref_count.fetch_add(1, Relaxed);
            if old_size == usize::MAX {
                abort();
            }

            Storage {
                arc: NonNull::new_unchecked(arc),
                ..*self
            }
        } else {
            assert!(kind == KIND_VEC);

            let vec_arc = arc.map_addr(|addr| addr & KIND_UNMASK) as *mut SharedVec;
            let old_size = (*vec_arc).ref_count.fetch_add(1, Relaxed);
            if old_size == usize::MAX {
                abort();
            }

            Storage {
                arc: NonNull::new_unchecked(arc),
                ..*self
            }
        }
    }

    #[inline]
    pub(crate) fn reserve(&mut self, additional: usize) {
        let len = self.len();
        let rem = self.capacity() - len;

        if additional <= rem {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.reserve_inner(additional)
    }

    fn reserve_inner(&mut self, additional: usize) {
        let len = self.len();
        let kind = self.kind();

        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Storage` struct fields will be gibberish.
        if kind == KIND_INLINE {
            let new_cap = len + additional;

            // Promote to a vector
            *self = Storage::from_slice_with_capacity(new_cap, self.as_ref());
            return;
        }

        // Reserving involves abandoning the currently shared buffer and
        // allocating a new vector with the requested capacity.
        let new_cap = len + additional;

        if kind == KIND_VEC {
            let vec = self.shared_vec();

            unsafe {
                let vec_cap = (*vec).cap - SHARED_VEC_SIZE;

                // First, try to reclaim the buffer. This is possible if the current
                // handle is the only outstanding handle pointing to the buffer.
                if vec_cap >= new_cap && (*vec).is_unique() {
                    // The capacity is sufficient, reclaim the buffer
                    let ptr = (vec as *mut u8).add(SHARED_VEC_SIZE);
                    ptr::copy(self.ptr, ptr, len);

                    self.ptr = ptr;
                    self.cap = vec_cap;
                } else {
                    // Create a new vector storage
                    *self = Storage::from_slice_with_capacity(new_cap, self.as_ref());
                }
            }
        } else {
            debug_assert!(kind == KIND_ARC);

            let arc = self.arc.as_ptr();
            unsafe {
                // First, try to reclaim the buffer. This is possible if the current
                // handle is the only outstanding handle pointing to the buffer.
                if (*arc).is_unique() {
                    // This is the only handle to the buffer. It can be reclaimed.
                    // However, before doing the work of copying data, check to make
                    // sure that the vector has enough capacity.
                    let v = &mut (*arc).vec;

                    if v.capacity() >= new_cap {
                        // The capacity is sufficient, reclaim the buffer
                        let ptr = v.as_mut_ptr();

                        ptr::copy(self.ptr, ptr, len);

                        self.ptr = ptr;
                        self.cap = v.capacity();
                        return;
                    }
                }

                // Create a new vector storage
                *self = Storage::from_slice_with_capacity(new_cap, self.as_ref());
            }
        }
    }

    /// Returns true if the buffer is stored inline
    #[inline]
    pub(crate) fn is_inline(&self) -> bool {
        self.kind() == KIND_INLINE
    }

    #[inline]
    fn shared_vec(&self) -> *mut SharedVec {
        self.arc.as_ptr().map_addr(|addr| addr & KIND_UNMASK) as *mut SharedVec
    }

    #[inline]
    fn kind(&self) -> usize {
        // This function is going to probably raise some eyebrows. The function
        // returns true if the buffer is stored inline. This is done by checking
        // the least significant bit in the `arc` field.
        //
        // Now, you may notice that `arc` is an `AtomicPtr` and this is
        // accessing it as a normal field without performing an atomic load...
        //
        // Again, the function only cares about the least significant bit, and
        // this bit is set when `Storage` is created and never changed after that.
        // All platforms have atomic "word" operations and won't randomly flip
        // bits, so even without any explicit atomic operations, reading the
        // flag will be correct.
        //
        // This is undefined behavior due to a data race, but experimental
        // evidence shows that it works in practice (discussion:
        // https://internals.rust-lang.org/t/bit-wise-reasoning-for-atomic-accesses/8853).
        //
        // This function is very critical performance wise as it is called for
        // every operation. Performing an atomic load would mess with the
        // compiler's ability to optimize. Simple benchmarks show up to a 10%
        // slowdown using a `Relaxed` atomic load on x86.

        #[cfg(target_endian = "little")]
        #[inline]
        fn imp(arc: *mut Shared) -> usize {
            (arc as usize) & KIND_MASK
        }

        #[cfg(target_endian = "big")]
        #[inline]
        fn imp(arc: *mut Shared) -> usize {
            unsafe {
                let p: *const usize = arc as *const usize;
                *p & KIND_MASK
            }
        }

        imp(self.arc.as_ptr())
    }

    pub(crate) fn info(&self) -> Info {
        let kind = self.kind();

        let (id, refs, capacity) = unsafe {
            if kind == KIND_VEC {
                let ptr = self.shared_vec();
                (ptr as usize, (*ptr).ref_count.load(Relaxed), (*ptr).cap)
            } else if kind == KIND_ARC {
                let ptr = self.arc.as_ptr();
                (
                    ptr as usize,
                    (*ptr).ref_count.load(Relaxed),
                    (*ptr).vec.capacity(),
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
        let kind = self.kind();

        if kind == KIND_VEC {
            release_shared_vec(self.shared_vec());
        } else if kind == KIND_ARC {
            release_shared(self.arc.as_ptr());
        }
    }
}

fn release_shared(ptr: *mut Shared) {
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
        let _ = Box::from_raw(ptr);
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
            *self.as_ptr().add(len) = n;
        }
    }

    pub(crate) fn len(&self) -> usize {
        unsafe { (*self.0.as_ptr()).len as usize }
    }

    pub(crate) fn capacity(&self) -> usize {
        unsafe { (*self.0.as_ptr()).cap - (*self.0.as_ptr()).offset as usize }
    }

    pub(crate) fn freeze(self) -> Storage {
        unsafe {
            if self.len() <= INLINE_CAP {
                Storage::from_ptr_inline(self.as_ptr(), self.len())
            } else {
                let inner = Storage {
                    ptr: self.as_ptr(),
                    len: self.len(),
                    cap: self.capacity(),
                    arc: NonNull::new_unchecked(
                        self.0.as_ptr().map_addr(|addr| addr ^ KIND_VEC) as *mut Shared,
                    ),
                };
                mem::forget(self);
                inner
            }
        }
    }

    pub(crate) fn with_bytes_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        unsafe {
            let len = self.len();
            let cap = self.capacity();

            let mut buf = BytesMut {
                storage: Storage {
                    len,
                    cap,
                    ptr: self.as_ptr(),
                    arc: NonNull::new_unchecked(
                        self.0.as_ptr().map_addr(|addr| addr ^ KIND_VEC) as *mut Shared,
                    ),
                },
            };

            let result = f(&mut buf);

            // convert BytesMut back to StorageVec
            // only KIND_VEC could be converted to self, otherwise we have to copy data
            let storage = match buf.storage.kind() {
                KIND_VEC => {
                    let ptr = buf.storage.shared_vec();
                    let offset = buf.storage.ptr as usize - ptr as usize;

                    // we cannot use shared vec if BytesMut points to inside of vec
                    if buf.storage.cap < (*ptr).cap - offset {
                        StorageVec::from_slice(buf.capacity(), buf.storage.as_ref())
                    } else {
                        // BytesMut owns rest of the vec, so it can be re-used
                        (*ptr).len = buf.len() as u32;
                        (*ptr).offset = offset as u32;
                        mem::forget(buf); // we dont want to run drop for Storage
                        StorageVec(NonNull::new_unchecked(ptr))
                    }
                }
                KIND_INLINE | KIND_STATIC | KIND_ARC => {
                    StorageVec::from_slice(buf.capacity(), buf.storage.as_ref())
                }
                _ => panic!(),
            };

            let old = mem::replace(self, storage);
            mem::forget(old);

            result
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

                Storage {
                    ptr,
                    len: at,
                    cap: at,
                    arc: NonNull::new_unchecked(
                        self.0.as_ptr().map_addr(|addr| addr ^ KIND_VEC) as *mut Shared,
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
                if inner.is_unique() && inner.offset != SHARED_VEC_SIZE as u32 {
                    inner.offset = SHARED_VEC_SIZE as u32;
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
        if additional <= self.capacity() - self.len() {
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
            let cap = inner.cap - SHARED_VEC_SIZE;

            // Reserving involves abandoning the currently shared buffer and
            // allocating a new vector with the requested capacity.
            let new_cap = len + additional;

            // try to reclaim the buffer. This is possible if the current
            // handle is the only outstanding handle pointing to the buffer.
            if cap >= new_cap && inner.is_unique() {
                let offset = inner.offset;
                inner.offset = SHARED_VEC_SIZE as u32;

                // The capacity is sufficient, reclaim the buffer
                if len != 0 {
                    let ptr = self.0.as_ptr() as *mut u8;
                    ptr::copy(ptr.add(offset as usize), ptr.add(SHARED_VEC_SIZE), len);
                }
            } else {
                // Create a new storage
                *self = StorageVec(SharedVec::create(new_cap, self.as_ref()));
            }
        }
    }

    /// slice.
    #[inline]
    pub(crate) unsafe fn set_len(&mut self, len: usize) {
        assert!(len <= self.capacity());
        self.0.as_mut().len = len as u32;
    }

    pub(crate) unsafe fn set_start(&mut self, start: u32) {
        if start != 0 {
            let cap = self.capacity();
            let inner = self.as_inner();

            assert!(
                start <= cap as u32,
                "Cannot set start position cap:{} offset:{} len:{} acap:{}",
                inner.cap,
                inner.offset,
                inner.len,
                cap
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
        }
    }
}

impl Drop for StorageVec {
    fn drop(&mut self) {
        release_shared_vec(self.0.as_ptr());
    }
}

impl Shared {
    fn is_unique(&self) -> bool {
        // The goal is to check if the current handle is the only handle
        // that currently has access to the buffer. This is done by
        // checking if the `ref_count` is currently 1.
        //
        // The `Acquire` ordering synchronizes with the `Release` as
        // part of the `fetch_sub` in `release_shared`. The `fetch_sub`
        // operation guarantees that any mutations done in other threads
        // are ordered before the `ref_count` is decremented. As such,
        // this `Acquire` will guarantee that those mutations are
        // visible to the current thread.
        self.ref_count.load(Acquire) == 1
    }
}

impl SharedVec {
    fn create(cap: usize, src: &[u8]) -> NonNull<SharedVec> {
        let ptr = Self::alloc_with_capacity(cap, src.len() as u32);

        // copy slice
        unsafe {
            let dst = ptr.add(SHARED_VEC_SIZE);
            let sl = slice::from_raw_parts_mut(dst, src.len());
            sl.copy_from_slice(src);
            NonNull::new_unchecked(ptr as *mut SharedVec)
        }
    }

    fn create_with_capacity(cap: usize) -> NonNull<SharedVec> {
        let ptr = Self::alloc_with_capacity(cap, 0);
        unsafe { NonNull::new_unchecked(ptr as *mut SharedVec) }
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
                    cap: layout.size(),
                    offset: SHARED_VEC_SIZE as u32,
                    ref_count: AtomicUsize::new(1),
                    data: [],
                },
            );
            ptr
        }
    }

    fn is_unique(&self) -> bool {
        // This is same as Shared::is_unique() but for KIND_VEC
        self.ref_count.load(Acquire) == 1
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
        let cap = (*ptr).cap;
        ptr::drop_in_place(ptr);
        let layout = shared_vec_layout(cap - SHARED_VEC_SIZE).unwrap();
        alloc::dealloc(ptr as *mut _, layout);
    }
}

const fn shared_vec_layout(cap: usize) -> Result<Layout, LayoutError> {
    let s_layout = Layout::new::<u8>();
    let size = s_layout.size() * cap;
    let s_layout = match Layout::from_size_align(size, s_layout.align()) {
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
            KIND_ARC => Kind::Arc,
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
    fn bytes_vec() {
        let bv = BytesVec::copy_from_slice(LONG);
        assert_eq!(bv.capacity(), 263);
        assert_eq!(bv.len(), 263);
        assert_eq!(bv.as_ref().len(), 263);
        assert_eq!(bv.as_ref(), LONG);
        assert_eq!(&bv[..], LONG);

        let sl: &[u8] = &[];
        let bv = BytesVec::copy_from_slice(sl);
        assert_eq!(bv.capacity(), 0);
        assert_eq!(bv.len(), 0);
        assert_eq!(bv.as_ref().len(), 0);
        assert_eq!(bv.as_ref(), sl);
        assert_eq!(&bv[..], sl);

        let mut bv = BytesVec::copy_from_slice(&b"hello"[..]);
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

        let b = Bytes::from(bv);
        assert_eq!(b, "hello world");

        let mut b = BytesMut::try_from(b).unwrap();
        b.put(".");
        assert_eq!(b, "hello world.");

        // does not re-alloc
        let mut bv = BytesVec::with_capacity(0);
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
        assert!(bv.capacity() == 48);
        let p2 = unsafe { bv.storage.as_ptr() as usize };
        assert!(p1 == p2);
    }
}
