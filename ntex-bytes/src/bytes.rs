use std::borrow::{Borrow, BorrowMut};
use std::ops::{Deref, DerefMut, RangeBounds};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{self, AtomicUsize};
use std::{cmp, fmt, hash, mem, ptr, ptr::NonNull, slice, usize};

use crate::pool::{PoolId, PoolRef};
use crate::{buf::IntoIter, buf::UninitSlice, debug, Buf, BufMut};

/// A reference counted contiguous slice of memory.
///
/// `Bytes` is an efficient container for storing and operating on contiguous
/// slices of memory. It is intended for use primarily in networking code, but
/// could have applications elsewhere as well.
///
/// `Bytes` values facilitate zero-copy network programming by allowing multiple
/// `Bytes` objects to point to the same underlying memory. This is managed by
/// using a reference count to track when the memory is no longer needed and can
/// be freed.
///
/// ```
/// use ntex_bytes::Bytes;
///
/// let mut mem = Bytes::from(&b"Hello world"[..]);
/// let a = mem.slice(0..5);
///
/// assert_eq!(a, b"Hello");
///
/// let b = mem.split_to(6);
///
/// assert_eq!(mem, b"world");
/// assert_eq!(b, b"Hello ");
/// ```
///
/// # Memory layout
///
/// The `Bytes` struct itself is fairly small, limited to a pointer to the
/// memory and 4 `usize` fields used to track information about which segment of
/// the underlying memory the `Bytes` handle has access to.
///
/// The memory layout looks like this:
///
/// ```text
/// +-------+
/// | Bytes |
/// +-------+
///  /      \_____
/// |              \
/// v               v
/// +-----+------------------------------------+
/// | Arc |         |      Data     |          |
/// +-----+------------------------------------+
/// ```
///
/// `Bytes` keeps both a pointer to the shared `Arc` containing the full memory
/// slice and a pointer to the start of the region visible by the handle.
/// `Bytes` also tracks the length of its view into the memory.
///
/// # Sharing
///
/// The memory itself is reference counted, and multiple `Bytes` objects may
/// point to the same region. Each `Bytes` handle point to different sections within
/// the memory region, and `Bytes` handle may or may not have overlapping views
/// into the memory.
///
///
/// ```text
///
///    Arc ptrs                   +---------+
///    ________________________ / | Bytes 2 |
///   /                           +---------+
///  /          +-----------+     |         |
/// |_________/ |  Bytes 1  |     |         |
/// |           +-----------+     |         |
/// |           |           | ___/ data     | tail
/// |      data |      tail |/              |
/// v           v           v               v
/// +-----+---------------------------------+-----+
/// | Arc |     |           |               |     |
/// +-----+---------------------------------+-----+
/// ```
///
/// # Mutating
///
/// While `Bytes` handles may potentially represent overlapping views of the
/// underlying memory slice and may not be mutated, `BytesMut` handles are
/// guaranteed to be the only handle able to view that slice of memory. As such,
/// `BytesMut` handles are able to mutate the underlying memory. Note that
/// holding a unique view to a region of memory does not mean that there are no
/// other `Bytes` and `BytesMut` handles with disjoint views of the underlying
/// memory.
///
/// # Inline bytes
///
/// As an optimization, when the slice referenced by a `Bytes` handle is small
/// enough [^1]. In this case, a clone is no longer "shallow" and the data will
/// be copied.  Converting from a `Vec` will never use inlining. `BytesMut` does
/// not support data inlining and always allocates, but during converion to `Bytes`
/// data from `BytesMut` could be inlined.
///
/// [^1]: Small enough: 31 bytes on 64 bit systems, 15 on 32 bit systems.
///
pub struct Bytes {
    inner: Inner,
}

/// A unique reference to a contiguous slice of memory.
///
/// `BytesMut` represents a unique view into a potentially shared memory region.
/// Given the uniqueness guarantee, owners of `BytesMut` handles are able to
/// mutate the memory. It is similar to a `Vec<u8>` but with less copies and
/// allocations.
///
/// For more detail, see [Bytes](struct.Bytes.html).
///
/// # Growth
///
/// One key difference from `Vec<u8>` is that most operations **do not
/// implicitly grow the buffer**. This means that calling `my_bytes.put("hello
/// world");` could panic if `my_bytes` does not have enough capacity. Before
/// writing to the buffer, ensure that there is enough remaining capacity by
/// calling `my_bytes.remaining_mut()`. In general, avoiding calls to `reserve`
/// is preferable.
///
/// The only exception is `extend` which implicitly reserves required capacity.
///
/// # Examples
///
/// ```
/// use ntex_bytes::{BytesMut, BufMut};
///
/// let mut buf = BytesMut::with_capacity(64);
///
/// buf.put_u8(b'h');
/// buf.put_u8(b'e');
/// buf.put("llo");
///
/// assert_eq!(buf, b"hello");
///
/// // Freeze the buffer so that it can be shared
/// let a = buf.freeze();
///
/// // This does not allocate, instead `b` points to the same memory.
/// let b = a.clone();
///
/// assert_eq!(a, b"hello");
/// assert_eq!(b, b"hello");
/// ```
pub struct BytesMut {
    inner: Inner,
}

/// A unique reference to a contiguous slice of memory.
///
/// `BytesVec` represents a unique view into a potentially shared memory region.
/// Given the uniqueness guarantee, owners of `BytesVec` handles are able to
/// mutate the memory. It is similar to a `Vec<u8>` but with less copies and
/// allocations. It also always allocates.
///
/// For more detail, see [Bytes](struct.Bytes.html).
///
/// # Growth
///
/// One key difference from `Vec<u8>` is that most operations **do not
/// implicitly grow the buffer**. This means that calling `my_bytes.put("hello
/// world");` could panic if `my_bytes` does not have enough capacity. Before
/// writing to the buffer, ensure that there is enough remaining capacity by
/// calling `my_bytes.remaining_mut()`. In general, avoiding calls to `reserve`
/// is preferable.
///
/// The only exception is `extend` which implicitly reserves required capacity.
///
/// # Examples
///
/// ```
/// use ntex_bytes::{BytesVec, BufMut};
///
/// let mut buf = BytesVec::with_capacity(64);
///
/// buf.put_u8(b'h');
/// buf.put_u8(b'e');
/// buf.put("llo");
///
/// assert_eq!(&buf[..], b"hello");
///
/// // Freeze the buffer so that it can be shared
/// let a = buf.freeze();
///
/// // This does not allocate, instead `b` points to the same memory.
/// let b = a.clone();
///
/// assert_eq!(a, b"hello");
/// assert_eq!(b, b"hello");
/// ```
pub struct BytesVec {
    inner: InnerVec,
}

// Both `Bytes` and `BytesMut` are backed by `Inner` and functions are delegated
// to `Inner` functions. The `Bytes` and `BytesMut` shims ensure that functions
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
// Both `Bytes` and `BytesMut` are wrappers around `Inner`, which provides the
// data fields as well as all of the function implementations.
//
// The `Inner` struct is carefully laid out in order to support the
// functionality described above as well as being as small as possible. Size is
// important as growing the size of the `Bytes` struct from 32 bytes to 40 bytes
// added as much as 15% overhead in benchmarks using `Bytes` in an HTTP header
// map structure.
//
// The `Inner` struct contains the following fields:
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
// When `Inner` is in allocated mode (backed by Vec<u8> or Arc<Vec<u8>>), this
// will be the pointer to the `Arc` structure tracking the ref count for the
// underlying buffer. When the pointer is null, then the `Arc` has not been
// allocated yet and `self` is the only outstanding handle for the underlying
// buffer.
//
// The lower two bits of `arc` are used to track the storage mode of `Inner`.
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
// casing the layout of `Inner` depending on if the target platform is big or
// little endian.
//
// On little endian platforms, the `arc` field must be the first field in the
// struct. On big endian platforms, the `arc` field must be the last field in
// the struct. Since a deterministic struct layout is required, `Inner` is
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
struct Inner {
    // WARNING: Do not access the fields directly unless you know what you are
    // doing. Instead, use the fns. See implementation comment above.
    arc: NonNull<Shared>,
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

#[cfg(target_endian = "big")]
#[repr(C)]
struct Inner {
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
// some of the logic around setting `Inner::arc` and other ways the `arc` field
// is used. Using `Arc` ended up requiring a number of funky transmutes and
// other shenanigans to make it work.
struct Shared {
    vec: Vec<u8>,
    ref_count: AtomicUsize,
    pool: PoolRef,
}

struct SharedVec {
    cap: usize,
    len: u32,
    offset: u32,
    ref_count: AtomicUsize,
    pool: PoolRef,
}

// Buffer storage strategy flags.
const KIND_ARC: usize = 0b00;
const KIND_INLINE: usize = 0b01;
const KIND_STATIC: usize = 0b10;
const KIND_VEC: usize = 0b11;
const KIND_MASK: usize = 0b11;
const KIND_UNMASK: usize = !KIND_MASK;

const MIN_NON_ZERO_CAP: usize = 64;
const SHARED_VEC_SIZE: usize = mem::size_of::<SharedVec>();

// Bit op constants for extracting the inline length value from the `arc` field.
const INLINE_LEN_MASK: usize = 0b1111_1100;
const INLINE_LEN_OFFSET: usize = 2;

// Byte offset from the start of `Inner` to where the inline buffer data
// starts. On little endian platforms, the first byte of the struct is the
// storage flag, so the data is shifted by a byte. On big endian systems, the
// data starts at the beginning of the struct.
#[cfg(target_endian = "little")]
const INLINE_DATA_OFFSET: isize = 2;
#[cfg(target_endian = "big")]
const INLINE_DATA_OFFSET: isize = 0;

// Inline buffer capacity. This is the size of `Inner` minus 1 byte for the
// metadata.
#[cfg(target_pointer_width = "64")]
const INLINE_CAP: usize = 4 * 8 - 2;
#[cfg(target_pointer_width = "32")]
const INLINE_CAP: usize = 4 * 4 - 2;

/*
 *
 * ===== Bytes =====
 *
 */

impl Bytes {
    /// Creates a new empty `Bytes`.
    ///
    /// This will not allocate and the returned `Bytes` handle will be empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let b = Bytes::new();
    /// assert_eq!(&b[..], b"");
    /// ```
    #[inline]
    pub const fn new() -> Bytes {
        Bytes {
            inner: Inner::empty_inline(),
        }
    }

    /// Creates a new `Bytes` from a static slice.
    ///
    /// The returned `Bytes` will point directly to the static slice. There is
    /// no allocating or copying.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let b = Bytes::from_static(b"hello");
    /// assert_eq!(&b[..], b"hello");
    /// ```
    #[inline]
    pub const fn from_static(bytes: &'static [u8]) -> Bytes {
        Bytes {
            inner: Inner::from_static(bytes),
        }
    }

    /// Returns the number of bytes contained in this `Bytes`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let b = Bytes::from(&b"hello"[..]);
    /// assert_eq!(b.len(), 5);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true if the `Bytes` has a length of 0.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let b = Bytes::new();
    /// assert!(b.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Return true if the `Bytes` uses inline allocation
    ///
    /// # Examples
    /// ```
    /// use ntex_bytes::{Bytes, BytesMut};
    ///
    /// assert!(Bytes::from(BytesMut::from(&[0, 0, 0, 0][..])).is_inline());
    /// assert!(Bytes::from(Vec::with_capacity(4)).is_inline());
    /// assert!(!Bytes::from(&[0; 1024][..]).is_inline());
    /// ```
    pub fn is_inline(&self) -> bool {
        self.inner.is_inline()
    }

    /// Creates `Bytes` instance from slice, by copying it.
    pub fn copy_from_slice(data: &[u8]) -> Self {
        Self::copy_from_slice_in(data, PoolId::DEFAULT)
    }

    /// Creates `Bytes` instance from slice, by copying it.
    pub fn copy_from_slice_in<T>(data: &[u8], pool: T) -> Self
    where
        PoolRef: From<T>,
    {
        if data.len() <= INLINE_CAP {
            Bytes {
                inner: Inner::from_slice_inline(data),
            }
        } else {
            Bytes {
                inner: Inner::from_slice(data.len(), data, pool.into()),
            }
        }
    }

    /// Returns a slice of self for the provided range.
    ///
    /// This will increment the reference count for the underlying memory and
    /// return a new `Bytes` handle set to the slice.
    ///
    /// This operation is `O(1)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let a = Bytes::from(b"hello world");
    /// let b = a.slice(2..5);
    ///
    /// assert_eq!(&b[..], b"llo");
    /// assert_eq!(&b[..=1], b"ll");
    /// assert_eq!(&b[1..=1], b"l");
    /// ```
    ///
    /// # Panics
    ///
    /// Requires that `begin <= end` and `end <= self.len()`, otherwise slicing
    /// will panic.
    pub fn slice(&self, range: impl RangeBounds<usize>) -> Bytes {
        use std::ops::Bound;

        let len = self.len();

        let begin = match range.start_bound() {
            Bound::Included(&n) => n,
            Bound::Excluded(&n) => n + 1,
            Bound::Unbounded => 0,
        };

        let end = match range.end_bound() {
            Bound::Included(&n) => n + 1,
            Bound::Excluded(&n) => n,
            Bound::Unbounded => len,
        };

        assert!(begin <= end);
        assert!(end <= len);

        if end - begin <= INLINE_CAP {
            Bytes {
                inner: Inner::from_slice_inline(&self[begin..end]),
            }
        } else {
            let mut ret = self.clone();

            unsafe {
                ret.inner.set_end(end);
                ret.inner.set_start(begin);
            }
            ret
        }
    }

    /// Returns a slice of self that is equivalent to the given `subset`.
    ///
    /// When processing a `Bytes` buffer with other tools, one often gets a
    /// `&[u8]` which is in fact a slice of the `Bytes`, i.e. a subset of it.
    /// This function turns that `&[u8]` into another `Bytes`, as if one had
    /// called `self.slice()` with the offsets that correspond to `subset`.
    ///
    /// This operation is `O(1)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let bytes = Bytes::from(&b"012345678"[..]);
    /// let as_slice = bytes.as_ref();
    /// let subset = &as_slice[2..6];
    /// let subslice = bytes.slice_ref(&subset);
    /// assert_eq!(subslice, b"2345");
    /// ```
    ///
    /// # Panics
    ///
    /// Requires that the given `sub` slice is in fact contained within the
    /// `Bytes` buffer; otherwise this function will panic.
    pub fn slice_ref(&self, subset: &[u8]) -> Bytes {
        let bytes_p = self.as_ptr() as usize;
        let bytes_len = self.len();

        let sub_p = subset.as_ptr() as usize;
        let sub_len = subset.len();

        assert!(sub_p >= bytes_p);
        assert!(sub_p + sub_len <= bytes_p + bytes_len);

        let sub_offset = sub_p - bytes_p;

        self.slice(sub_offset..(sub_offset + sub_len))
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Afterwards `self` contains elements `[0, at)`, and the returned `Bytes`
    /// contains elements `[at, len)`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let mut a = Bytes::from(&b"hello world"[..]);
    /// let b = a.split_off(5);
    ///
    /// assert_eq!(a, b"hello");
    /// assert_eq!(b, b" world");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `at > len`.
    pub fn split_off(&mut self, at: usize) -> Bytes {
        assert!(at <= self.len());

        if at == self.len() {
            return Bytes::new();
        }

        if at == 0 {
            mem::replace(self, Bytes::new())
        } else {
            Bytes {
                inner: self.inner.split_off(at, true),
            }
        }
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Afterwards `self` contains elements `[at, len)`, and the returned
    /// `Bytes` contains elements `[0, at)`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let mut a = Bytes::from(&b"hello world"[..]);
    /// let b = a.split_to(5);
    ///
    /// assert_eq!(a, b" world");
    /// assert_eq!(b, b"hello");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `at > len`.
    pub fn split_to(&mut self, at: usize) -> Bytes {
        assert!(at <= self.len());

        if at == self.len() {
            return mem::replace(self, Bytes::new());
        }

        if at == 0 {
            Bytes::new()
        } else {
            Bytes {
                inner: self.inner.split_to(at, true),
            }
        }
    }

    /// Shortens the buffer, keeping the first `len` bytes and dropping the
    /// rest.
    ///
    /// If `len` is greater than the buffer's current length, this has no
    /// effect.
    ///
    /// The [`split_off`] method can emulate `truncate`, but this causes the
    /// excess bytes to be returned instead of dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let mut buf = Bytes::from(&b"hello world"[..]);
    /// buf.truncate(5);
    /// assert_eq!(buf, b"hello"[..]);
    /// ```
    ///
    /// [`split_off`]: #method.split_off
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.inner.truncate(len, true);
    }

    /// Shortens the buffer to `len` bytes and dropping the rest.
    ///
    /// This is useful if underlying buffer is larger than cuurrent bytes object.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let mut buf = Bytes::from(&b"hello world"[..]);
    /// buf.trimdown();
    /// assert_eq!(buf, b"hello world"[..]);
    /// ```
    #[inline]
    pub fn trimdown(&mut self) {
        let kind = self.inner.kind();

        // trim down only if buffer is not inline or static and
        // buffer's unused space is greater than 64 bytes
        if !(kind == KIND_INLINE || kind == KIND_STATIC) {
            if self.inner.len() <= INLINE_CAP {
                *self = Bytes {
                    inner: Inner::from_slice_inline(self),
                };
            } else if self.inner.capacity() - self.inner.len() >= 64 {
                *self = Bytes {
                    inner: Inner::from_slice(self.len(), self, self.inner.pool()),
                }
            }
        }
    }

    /// Clears the buffer, removing all data.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let mut buf = Bytes::from(&b"hello world"[..]);
    /// buf.clear();
    /// assert!(buf.is_empty());
    /// ```
    #[inline]
    pub fn clear(&mut self) {
        self.inner = Inner::empty_inline();
    }

    /// Attempts to convert into a `BytesMut` handle.
    ///
    /// This will only succeed if there are no other outstanding references to
    /// the underlying chunk of memory. `Bytes` handles that contain inlined
    /// bytes will always be convertible to `BytesMut`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let a = Bytes::copy_from_slice(&b"Mary had a little lamb, little lamb, little lamb..."[..]);
    ///
    /// // Create a shallow clone
    /// let b = a.clone();
    ///
    /// // This will fail because `b` shares a reference with `a`
    /// let a = a.try_mut().unwrap_err();
    ///
    /// drop(b);
    ///
    /// // This will succeed
    /// let mut a = a.try_mut().unwrap();
    ///
    /// a[0] = b'b';
    ///
    /// assert_eq!(&a[..4], b"bary");
    /// ```
    pub fn try_mut(self) -> Result<BytesMut, Bytes> {
        if self.inner.is_mut_safe() {
            Ok(BytesMut { inner: self.inner })
        } else {
            Err(self)
        }
    }

    /// Returns an iterator over the bytes contained by the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{Buf, Bytes};
    ///
    /// let buf = Bytes::from(&b"abc"[..]);
    /// let mut iter = buf.iter();
    ///
    /// assert_eq!(iter.next().map(|b| *b), Some(b'a'));
    /// assert_eq!(iter.next().map(|b| *b), Some(b'b'));
    /// assert_eq!(iter.next().map(|b| *b), Some(b'c'));
    /// assert_eq!(iter.next(), None);
    /// ```
    pub fn iter(&'_ self) -> std::slice::Iter<'_, u8> {
        self.chunk().iter()
    }
}

impl Buf for Bytes {
    #[inline]
    fn remaining(&self) -> usize {
        self.len()
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        self.inner.as_ref()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        assert!(
            cnt <= self.inner.as_ref().len(),
            "cannot advance past `remaining`"
        );
        unsafe {
            self.inner.set_start(cnt);
        }
    }
}

impl bytes::buf::Buf for Bytes {
    #[inline]
    fn remaining(&self) -> usize {
        self.len()
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        self.inner.as_ref()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        assert!(
            cnt <= self.inner.as_ref().len(),
            "cannot advance past `remaining`"
        );
        unsafe {
            self.inner.set_start(cnt);
        }
    }
}

impl Clone for Bytes {
    fn clone(&self) -> Bytes {
        Bytes {
            inner: unsafe { self.inner.shallow_clone() },
        }
    }
}

impl AsRef<[u8]> for Bytes {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl Deref for Bytes {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl From<&Bytes> for Bytes {
    fn from(src: &Bytes) -> Bytes {
        src.clone()
    }
}

impl From<BytesMut> for Bytes {
    fn from(src: BytesMut) -> Bytes {
        src.freeze()
    }
}

impl From<Vec<u8>> for Bytes {
    /// Convert a `Vec` into a `Bytes`
    ///
    /// This constructor may be used to avoid the inlining optimization used by
    /// `with_capacity`.  A `Bytes` constructed this way will always store its
    /// data on the heap.
    fn from(src: Vec<u8>) -> Bytes {
        if src.is_empty() {
            Bytes::new()
        } else if src.len() <= INLINE_CAP {
            Bytes {
                inner: Inner::from_slice_inline(&src),
            }
        } else {
            BytesMut::from(src).freeze()
        }
    }
}

impl From<String> for Bytes {
    fn from(src: String) -> Bytes {
        if src.is_empty() {
            Bytes::new()
        } else if src.bytes().len() <= INLINE_CAP {
            Bytes {
                inner: Inner::from_slice_inline(src.as_bytes()),
            }
        } else {
            BytesMut::from(src).freeze()
        }
    }
}

impl From<&'static [u8]> for Bytes {
    fn from(src: &'static [u8]) -> Bytes {
        Bytes::from_static(src)
    }
}

impl From<&'static str> for Bytes {
    fn from(src: &'static str) -> Bytes {
        Bytes::from_static(src.as_bytes())
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for Bytes {
    fn from(src: &'a [u8; N]) -> Bytes {
        Bytes::copy_from_slice(src)
    }
}

impl FromIterator<u8> for Bytes {
    fn from_iter<T: IntoIterator<Item = u8>>(into_iter: T) -> Self {
        BytesMut::from_iter(into_iter).freeze()
    }
}

impl<'a> FromIterator<&'a u8> for Bytes {
    fn from_iter<T: IntoIterator<Item = &'a u8>>(into_iter: T) -> Self {
        BytesMut::from_iter(into_iter).freeze()
    }
}

impl Eq for Bytes {}

impl PartialEq for Bytes {
    fn eq(&self, other: &Bytes) -> bool {
        self.inner.as_ref() == other.inner.as_ref()
    }
}

impl PartialOrd for Bytes {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bytes {
    fn cmp(&self, other: &Bytes) -> cmp::Ordering {
        self.inner.as_ref().cmp(other.inner.as_ref())
    }
}

impl Default for Bytes {
    #[inline]
    fn default() -> Bytes {
        Bytes::new()
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&debug::BsDebug(self.inner.as_ref()), fmt)
    }
}

impl hash::Hash for Bytes {
    fn hash<H>(&self, state: &mut H)
    where
        H: hash::Hasher,
    {
        let s: &[u8] = self.as_ref();
        s.hash(state);
    }
}

impl Borrow<[u8]> for Bytes {
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl IntoIterator for Bytes {
    type Item = u8;
    type IntoIter = IntoIter<Bytes>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter::new(self)
    }
}

impl<'a> IntoIterator for &'a Bytes {
    type Item = &'a u8;
    type IntoIter = std::slice::Iter<'a, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_ref().iter()
    }
}

/*
 *
 * ===== BytesMut =====
 *
 */

impl BytesMut {
    /// Creates a new `BytesMut` with the specified capacity.
    ///
    /// The returned `BytesMut` will be able to hold at least `capacity` bytes
    /// without reallocating. If `capacity` is under `4 * size_of::<usize>() - 1`,
    /// then `BytesMut` will not allocate.
    ///
    /// It is important to note that this function does not specify the length
    /// of the returned `BytesMut`, but only the capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` greater than 60bit for 64bit systems
    /// and 28bit for 32bit systems
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesMut, BufMut};
    ///
    /// let mut bytes = BytesMut::with_capacity(64);
    ///
    /// // `bytes` contains no data, even though there is capacity
    /// assert_eq!(bytes.len(), 0);
    ///
    /// bytes.put(&b"hello world"[..]);
    ///
    /// assert_eq!(&bytes[..], b"hello world");
    /// ```
    #[inline]
    pub fn with_capacity(capacity: usize) -> BytesMut {
        Self::with_capacity_in(capacity, PoolId::DEFAULT.pool_ref())
    }

    /// Creates a new `BytesMut` with the specified capacity and in specified memory pool.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesMut, BufMut, PoolId};
    ///
    /// let mut bytes = BytesMut::with_capacity_in(64, PoolId::P1);
    ///
    /// // `bytes` contains no data, even though there is capacity
    /// assert_eq!(bytes.len(), 0);
    ///
    /// bytes.put(&b"hello world"[..]);
    ///
    /// assert_eq!(&bytes[..], b"hello world");
    /// assert!(PoolId::P1.pool_ref().allocated() > 0);
    /// ```
    #[inline]
    pub fn with_capacity_in<T>(capacity: usize, pool: T) -> BytesMut
    where
        PoolRef: From<T>,
    {
        BytesMut {
            inner: Inner::with_capacity(capacity, pool.into()),
        }
    }

    /// Creates a new `BytesMut` from slice, by copying it.
    pub fn copy_from_slice<T: AsRef<[u8]>>(src: T) -> Self {
        Self::copy_from_slice_in(src, PoolId::DEFAULT)
    }

    /// Creates a new `BytesMut` from slice, by copying it.
    pub fn copy_from_slice_in<T, U>(src: T, pool: U) -> Self
    where
        T: AsRef<[u8]>,
        PoolRef: From<U>,
    {
        let s = src.as_ref();
        BytesMut {
            inner: Inner::from_slice(s.len(), s, pool.into()),
        }
    }

    #[inline]
    /// Convert a `Vec` into a `BytesMut`
    pub fn from_vec<T>(src: Vec<u8>, pool: T) -> BytesMut
    where
        PoolRef: From<T>,
    {
        BytesMut {
            inner: Inner::from_vec(src, pool.into()),
        }
    }

    /// Creates a new `BytesMut` with default capacity.
    ///
    /// Resulting object has length 0 and unspecified capacity.
    /// This function does not allocate.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesMut, BufMut};
    ///
    /// let mut bytes = BytesMut::new();
    ///
    /// assert_eq!(0, bytes.len());
    ///
    /// bytes.reserve(2);
    /// bytes.put_slice(b"xy");
    ///
    /// assert_eq!(&b"xy"[..], &bytes[..]);
    /// ```
    #[inline]
    pub fn new() -> BytesMut {
        BytesMut::with_capacity(MIN_NON_ZERO_CAP)
    }

    /// Returns the number of bytes contained in this `BytesMut`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let b = BytesMut::from(&b"hello"[..]);
    /// assert_eq!(b.len(), 5);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true if the `BytesMut` has a length of 0.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let b = BytesMut::with_capacity(64);
    /// assert!(b.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the number of bytes the `BytesMut` can hold without reallocating.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let b = BytesMut::with_capacity(64);
    /// assert_eq!(b.capacity(), 64);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Converts `self` into an immutable `Bytes`.
    ///
    /// The conversion is zero cost and is used to indicate that the slice
    /// referenced by the handle will no longer be mutated. Once the conversion
    /// is done, the handle can be cloned and shared across threads.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesMut, BufMut};
    /// use std::thread;
    ///
    /// let mut b = BytesMut::with_capacity(64);
    /// b.put("hello world");
    /// let b1 = b.freeze();
    /// let b2 = b1.clone();
    ///
    /// let th = thread::spawn(move || {
    ///     assert_eq!(b1, b"hello world");
    /// });
    ///
    /// assert_eq!(b2, b"hello world");
    /// th.join().unwrap();
    /// ```
    #[inline]
    pub fn freeze(self) -> Bytes {
        if self.inner.len() <= INLINE_CAP {
            Bytes {
                inner: Inner::from_slice_inline(self.inner.as_ref()),
            }
        } else {
            Bytes { inner: self.inner }
        }
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Afterwards `self` contains elements `[0, at)`, and the returned
    /// `BytesMut` contains elements `[at, capacity)`.
    ///
    /// This is an `O(1)` operation that just increases the reference count
    /// and sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut a = BytesMut::from(&b"hello world"[..]);
    /// let mut b = a.split_off(5);
    ///
    /// a[0] = b'j';
    /// b[0] = b'!';
    ///
    /// assert_eq!(&a[..], b"jello");
    /// assert_eq!(&b[..], b"!world");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `at > capacity`.
    pub fn split_off(&mut self, at: usize) -> BytesMut {
        BytesMut {
            inner: self.inner.split_off(at, false),
        }
    }

    /// Removes the bytes from the current view, returning them in a new
    /// `BytesMut` handle.
    ///
    /// Afterwards, `self` will be empty, but will retain any additional
    /// capacity that it had before the operation. This is identical to
    /// `self.split_to(self.len())`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesMut, BufMut};
    ///
    /// let mut buf = BytesMut::with_capacity(1024);
    /// buf.put(&b"hello world"[..]);
    ///
    /// let other = buf.split();
    ///
    /// assert!(buf.is_empty());
    /// assert_eq!(1013, buf.capacity());
    ///
    /// assert_eq!(other, b"hello world"[..]);
    /// ```
    pub fn split(&mut self) -> BytesMut {
        let len = self.len();
        self.split_to(len)
    }

    /// Splits the buffer into two at the given index.
    ///
    /// Afterwards `self` contains elements `[at, len)`, and the returned `BytesMut`
    /// contains elements `[0, at)`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut a = BytesMut::from(&b"hello world"[..]);
    /// let mut b = a.split_to(5);
    ///
    /// a[0] = b'!';
    /// b[0] = b'j';
    ///
    /// assert_eq!(&a[..], b"!world");
    /// assert_eq!(&b[..], b"jello");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `at > len`.
    pub fn split_to(&mut self, at: usize) -> BytesMut {
        assert!(at <= self.len());

        BytesMut {
            inner: self.inner.split_to(at, false),
        }
    }

    /// Shortens the buffer, keeping the first `len` bytes and dropping the
    /// rest.
    ///
    /// If `len` is greater than the buffer's current length, this has no
    /// effect.
    ///
    /// The [`split_off`] method can emulate `truncate`, but this causes the
    /// excess bytes to be returned instead of dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::from(&b"hello world"[..]);
    /// buf.truncate(5);
    /// assert_eq!(buf, b"hello"[..]);
    /// ```
    ///
    /// [`split_off`]: #method.split_off
    pub fn truncate(&mut self, len: usize) {
        self.inner.truncate(len, false);
    }

    /// Clears the buffer, removing all data.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::from(&b"hello world"[..]);
    /// buf.clear();
    /// assert!(buf.is_empty());
    /// ```
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Resizes the buffer so that `len` is equal to `new_len`.
    ///
    /// If `new_len` is greater than `len`, the buffer is extended by the
    /// difference with each additional byte set to `value`. If `new_len` is
    /// less than `len`, the buffer is simply truncated.
    ///
    /// # Panics
    ///
    /// Panics if `new_len` greater than 60bit for 64bit systems
    /// and 28bit for 32bit systems
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::new();
    ///
    /// buf.resize(3, 0x1);
    /// assert_eq!(&buf[..], &[0x1, 0x1, 0x1]);
    ///
    /// buf.resize(2, 0x2);
    /// assert_eq!(&buf[..], &[0x1, 0x1]);
    ///
    /// buf.resize(4, 0x3);
    /// assert_eq!(&buf[..], &[0x1, 0x1, 0x3, 0x3]);
    /// ```
    #[inline]
    pub fn resize(&mut self, new_len: usize, value: u8) {
        self.inner.resize(new_len, value);
    }

    /// Sets the length of the buffer.
    ///
    /// This will explicitly set the size of the buffer without actually
    /// modifying the data, so it is up to the caller to ensure that the data
    /// has been initialized.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut b = BytesMut::from(&b"hello world"[..]);
    ///
    /// unsafe {
    ///     b.set_len(5);
    /// }
    ///
    /// assert_eq!(&b[..], b"hello");
    ///
    /// unsafe {
    ///     b.set_len(11);
    /// }
    ///
    /// assert_eq!(&b[..], b"hello world");
    /// ```
    ///
    /// # Panics
    ///
    /// This method will panic if `len` is out of bounds for the underlying
    /// slice or if it comes after the `end` of the configured window.
    #[inline]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn set_len(&mut self, len: usize) {
        self.inner.set_len(len)
    }

    /// Reserves capacity for at least `additional` more bytes to be inserted
    /// into the given `BytesMut`.
    ///
    /// More than `additional` bytes may be reserved in order to avoid frequent
    /// reallocations. A call to `reserve` may result in an allocation.
    ///
    /// Before allocating new buffer space, the function will attempt to reclaim
    /// space in the existing buffer. If the current handle references a small
    /// view in the original buffer and all other handles have been dropped,
    /// and the requested capacity is less than or equal to the existing
    /// buffer's capacity, then the current view will be copied to the front of
    /// the buffer and the handle will take ownership of the full buffer.
    ///
    /// # Panics
    ///
    /// Panics if new capacity is greater than 60bit for 64bit systems
    /// and 28bit for 32bit systems
    ///
    /// # Examples
    ///
    /// In the following example, a new buffer is allocated.
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::from(&b"hello"[..]);
    /// buf.reserve(64);
    /// assert!(buf.capacity() >= 69);
    /// ```
    ///
    /// In the following example, the existing buffer is reclaimed.
    ///
    /// ```
    /// use ntex_bytes::{BytesMut, BufMut};
    ///
    /// let mut buf = BytesMut::with_capacity(128);
    /// buf.put(&[0; 64][..]);
    ///
    /// let ptr = buf.as_ptr();
    /// let other = buf.split();
    ///
    /// assert!(buf.is_empty());
    /// assert_eq!(buf.capacity(), 64);
    ///
    /// drop(other);
    /// buf.reserve(128);
    ///
    /// assert_eq!(buf.capacity(), 128);
    /// assert_eq!(buf.as_ptr(), ptr);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the new capacity overflows `usize`.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        let len = self.len();
        let rem = self.capacity() - len;

        if additional <= rem {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.inner.reserve_inner(additional);
    }

    /// Appends given bytes to this object.
    ///
    /// If this `BytesMut` object has not enough capacity, it is resized first.
    /// So unlike `put_slice` operation, `extend_from_slice` does not panic.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::with_capacity(0);
    /// buf.extend_from_slice(b"aaabbb");
    /// buf.extend_from_slice(b"cccddd");
    ///
    /// assert_eq!(b"aaabbbcccddd", &buf[..]);
    /// ```
    #[inline]
    pub fn extend_from_slice(&mut self, extend: &[u8]) {
        self.put_slice(extend);
    }

    /// Returns an iterator over the bytes contained by the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{Buf, BytesMut};
    ///
    /// let buf = BytesMut::from(&b"abc"[..]);
    /// let mut iter = buf.iter();
    ///
    /// assert_eq!(iter.next().map(|b| *b), Some(b'a'));
    /// assert_eq!(iter.next().map(|b| *b), Some(b'b'));
    /// assert_eq!(iter.next().map(|b| *b), Some(b'c'));
    /// assert_eq!(iter.next(), None);
    /// ```
    #[inline]
    pub fn iter(&'_ self) -> std::slice::Iter<'_, u8> {
        self.chunk().iter()
    }

    pub(crate) fn move_to_pool(&mut self, pool: PoolRef) {
        self.inner.move_to_pool(pool);
    }
}

impl Buf for BytesMut {
    #[inline]
    fn remaining(&self) -> usize {
        self.len()
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        self.inner.as_ref()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        assert!(
            cnt <= self.inner.as_ref().len(),
            "cannot advance past `remaining`"
        );
        unsafe {
            self.inner.set_start(cnt);
        }
    }
}

impl BufMut for BytesMut {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.capacity() - self.len()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        let new_len = self.len() + cnt;

        // This call will panic if `cnt` is too big
        self.inner.set_len(new_len);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        let len = self.len();

        unsafe {
            // This will never panic as `len` can never become invalid
            let ptr = &mut self.inner.as_raw()[len..];

            UninitSlice::from_raw_parts_mut(ptr.as_mut_ptr(), self.capacity() - len)
        }
    }

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        let len = src.len();
        self.reserve(len);

        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), self.chunk_mut().as_mut_ptr(), len);
            self.advance_mut(len);
        }
    }

    #[inline]
    fn put_u8(&mut self, n: u8) {
        self.reserve(1);
        self.inner.put_u8(n);
    }

    #[inline]
    fn put_i8(&mut self, n: i8) {
        self.reserve(1);
        self.put_u8(n as u8);
    }
}

impl bytes::buf::Buf for BytesMut {
    #[inline]
    fn remaining(&self) -> usize {
        self.len()
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        self.inner.as_ref()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        Buf::advance(self, cnt)
    }
}

unsafe impl bytes::buf::BufMut for BytesMut {
    #[inline]
    fn remaining_mut(&self) -> usize {
        BufMut::remaining_mut(self)
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        BufMut::advance_mut(self, cnt)
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        let len = self.len();
        unsafe {
            // This will never panic as `len` can never become invalid
            let ptr = &mut self.inner.as_raw()[len..];
            bytes::buf::UninitSlice::from_raw_parts_mut(
                ptr.as_mut_ptr(),
                self.capacity() - len,
            )
        }
    }

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        BufMut::put_slice(self, src)
    }

    #[inline]
    fn put_u8(&mut self, n: u8) {
        BufMut::put_u8(self, n)
    }

    #[inline]
    fn put_i8(&mut self, n: i8) {
        BufMut::put_i8(self, n)
    }
}

impl AsRef<[u8]> for BytesMut {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl AsMut<[u8]> for BytesMut {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.inner.as_mut()
    }
}

impl Deref for BytesMut {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl DerefMut for BytesMut {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.inner.as_mut()
    }
}

impl From<Vec<u8>> for BytesMut {
    #[inline]
    /// Convert a `Vec` into a `BytesMut`
    ///
    /// This constructor may be used to avoid the inlining optimization used by
    /// `with_capacity`.  A `BytesMut` constructed this way will always store
    /// its data on the heap.
    fn from(src: Vec<u8>) -> BytesMut {
        BytesMut::from_vec(src, PoolId::DEFAULT.pool_ref())
    }
}

impl From<String> for BytesMut {
    #[inline]
    fn from(src: String) -> BytesMut {
        BytesMut::from_vec(src.into_bytes(), PoolId::DEFAULT.pool_ref())
    }
}

impl<'a> From<&'a [u8]> for BytesMut {
    fn from(src: &'a [u8]) -> BytesMut {
        if src.is_empty() {
            BytesMut::new()
        } else {
            BytesMut::copy_from_slice_in(src, PoolId::DEFAULT.pool_ref())
        }
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for BytesMut {
    fn from(src: &'a [u8; N]) -> BytesMut {
        BytesMut::copy_from_slice_in(src, PoolId::DEFAULT.pool_ref())
    }
}

impl<'a> From<&'a str> for BytesMut {
    #[inline]
    fn from(src: &'a str) -> BytesMut {
        BytesMut::from(src.as_bytes())
    }
}

impl From<Bytes> for BytesMut {
    #[inline]
    fn from(src: Bytes) -> BytesMut {
        src.try_mut()
            .unwrap_or_else(|src| BytesMut::copy_from_slice_in(&src[..], src.inner.pool()))
    }
}

impl Eq for BytesMut {}

impl PartialEq for BytesMut {
    #[inline]
    fn eq(&self, other: &BytesMut) -> bool {
        self.inner.as_ref() == other.inner.as_ref()
    }
}

impl Default for BytesMut {
    #[inline]
    fn default() -> BytesMut {
        BytesMut::new()
    }
}

impl Borrow<[u8]> for BytesMut {
    #[inline]
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl BorrowMut<[u8]> for BytesMut {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8] {
        self.as_mut()
    }
}

impl fmt::Debug for BytesMut {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&debug::BsDebug(self.inner.as_ref()), fmt)
    }
}

impl fmt::Write for BytesMut {
    #[inline]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.remaining_mut() >= s.len() {
            self.put_slice(s.as_bytes());
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }

    #[inline]
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        fmt::write(self, args)
    }
}

impl Clone for BytesMut {
    #[inline]
    fn clone(&self) -> BytesMut {
        BytesMut::from(&self[..])
    }
}

impl IntoIterator for BytesMut {
    type Item = u8;
    type IntoIter = IntoIter<BytesMut>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter::new(self)
    }
}

impl<'a> IntoIterator for &'a BytesMut {
    type Item = &'a u8;
    type IntoIter = std::slice::Iter<'a, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_ref().iter()
    }
}

impl FromIterator<u8> for BytesMut {
    fn from_iter<T: IntoIterator<Item = u8>>(into_iter: T) -> Self {
        let iter = into_iter.into_iter();
        let (min, maybe_max) = iter.size_hint();

        let mut out = BytesMut::with_capacity(maybe_max.unwrap_or(min));
        for i in iter {
            out.reserve(1);
            out.put_u8(i);
        }

        out
    }
}

impl<'a> FromIterator<&'a u8> for BytesMut {
    fn from_iter<T: IntoIterator<Item = &'a u8>>(into_iter: T) -> Self {
        into_iter.into_iter().copied().collect::<BytesMut>()
    }
}

impl Extend<u8> for BytesMut {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = u8>,
    {
        let iter = iter.into_iter();

        let (lower, _) = iter.size_hint();
        self.reserve(lower);

        for b in iter {
            self.put_u8(b);
        }
    }
}

impl<'a> Extend<&'a u8> for BytesMut {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = &'a u8>,
    {
        self.extend(iter.into_iter().copied())
    }
}

/*
 *
 * ===== BytesVec =====
 *
 */

impl BytesVec {
    /// Creates a new `BytesVec` with the specified capacity.
    ///
    /// The returned `BytesVec` will be able to hold at least `capacity` bytes
    /// without reallocating.
    ///
    /// It is important to note that this function does not specify the length
    /// of the returned `BytesVec`, but only the capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` greater than 60bit for 64bit systems
    /// and 28bit for 32bit systems
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesVec, BufMut};
    ///
    /// let mut bytes = BytesVec::with_capacity(64);
    ///
    /// // `bytes` contains no data, even though there is capacity
    /// assert_eq!(bytes.len(), 0);
    ///
    /// bytes.put(&b"hello world"[..]);
    ///
    /// assert_eq!(&bytes[..], b"hello world");
    /// ```
    #[inline]
    pub fn with_capacity(capacity: usize) -> BytesVec {
        Self::with_capacity_in(capacity, PoolId::DEFAULT.pool_ref())
    }

    /// Creates a new `BytesVec` with the specified capacity and in specified memory pool.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesVec, BufMut, PoolId};
    ///
    /// let mut bytes = BytesVec::with_capacity_in(64, PoolId::P1);
    ///
    /// // `bytes` contains no data, even though there is capacity
    /// assert_eq!(bytes.len(), 0);
    ///
    /// bytes.put(&b"hello world"[..]);
    ///
    /// assert_eq!(&bytes[..], b"hello world");
    /// assert!(PoolId::P1.pool_ref().allocated() > 0);
    /// ```
    #[inline]
    pub fn with_capacity_in<T>(capacity: usize, pool: T) -> BytesVec
    where
        PoolRef: From<T>,
    {
        BytesVec {
            inner: InnerVec::with_capacity(capacity, pool.into()),
        }
    }

    /// Creates a new `BytesVec` from slice, by copying it.
    pub fn copy_from_slice<T: AsRef<[u8]>>(src: T) -> Self {
        Self::copy_from_slice_in(src, PoolId::DEFAULT)
    }

    /// Creates a new `BytesVec` from slice, by copying it.
    pub fn copy_from_slice_in<T, U>(src: T, pool: U) -> Self
    where
        T: AsRef<[u8]>,
        PoolRef: From<U>,
    {
        let s = src.as_ref();
        BytesVec {
            inner: InnerVec::from_slice(s.len(), s, pool.into()),
        }
    }

    /// Creates a new `BytesVec` with default capacity.
    ///
    /// Resulting object has length 0 and unspecified capacity.
    /// This function does not allocate.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesVec, BufMut};
    ///
    /// let mut bytes = BytesVec::new();
    ///
    /// assert_eq!(0, bytes.len());
    ///
    /// bytes.reserve(2);
    /// bytes.put_slice(b"xy");
    ///
    /// assert_eq!(&b"xy"[..], &bytes[..]);
    /// ```
    #[inline]
    pub fn new() -> BytesVec {
        BytesVec::with_capacity(MIN_NON_ZERO_CAP)
    }

    /// Returns the number of bytes contained in this `BytesVec`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let b = BytesVec::copy_from_slice(&b"hello"[..]);
    /// assert_eq!(b.len(), 5);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns true if the `BytesVec` has a length of 0.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let b = BytesVec::with_capacity(64);
    /// assert!(b.is_empty());
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.len() == 0
    }

    /// Returns the number of bytes the `BytesVec` can hold without reallocating.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let b = BytesVec::with_capacity(64);
    /// assert_eq!(b.capacity(), 64);
    /// ```
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Converts `self` into an immutable `Bytes`.
    ///
    /// The conversion is zero cost and is used to indicate that the slice
    /// referenced by the handle will no longer be mutated. Once the conversion
    /// is done, the handle can be cloned and shared across threads.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesVec, BufMut};
    /// use std::thread;
    ///
    /// let mut b = BytesVec::with_capacity(64);
    /// b.put("hello world");
    /// let b1 = b.freeze();
    /// let b2 = b1.clone();
    ///
    /// let th = thread::spawn(move || {
    ///     assert_eq!(b1, b"hello world");
    /// });
    ///
    /// assert_eq!(b2, b"hello world");
    /// th.join().unwrap();
    /// ```
    #[inline]
    pub fn freeze(self) -> Bytes {
        Bytes {
            inner: self.inner.into_inner(),
        }
    }

    /// Removes the bytes from the current view, returning them in a new
    /// `Bytes` instance.
    ///
    /// Afterwards, `self` will be empty, but will retain any additional
    /// capacity that it had before the operation. This is identical to
    /// `self.split_to(self.len())`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytesVec, BufMut};
    ///
    /// let mut buf = BytesVec::with_capacity(1024);
    /// buf.put(&b"hello world"[..]);
    ///
    /// let other = buf.split();
    ///
    /// assert!(buf.is_empty());
    /// assert_eq!(1013, buf.capacity());
    ///
    /// assert_eq!(other, b"hello world"[..]);
    /// ```
    pub fn split(&mut self) -> BytesMut {
        self.split_to(self.len())
    }

    /// Splits the buffer into two at the given index.
    ///
    /// Afterwards `self` contains elements `[at, len)`, and the returned `Bytes`
    /// contains elements `[0, at)`.
    ///
    /// This is an `O(1)` operation that just increases the reference count and
    /// sets a few indices.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let mut a = BytesVec::copy_from_slice(&b"hello world"[..]);
    /// let mut b = a.split_to(5);
    ///
    /// a[0] = b'!';
    ///
    /// assert_eq!(&a[..], b"!world");
    /// assert_eq!(&b[..], b"hello");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `at > len`.
    pub fn split_to(&mut self, at: usize) -> BytesMut {
        assert!(at <= self.len());

        BytesMut {
            inner: self.inner.split_to(at, false),
        }
    }

    /// Shortens the buffer, keeping the first `len` bytes and dropping the
    /// rest.
    ///
    /// If `len` is greater than the buffer's current length, this has no
    /// effect.
    ///
    /// The [`split_off`] method can emulate `truncate`, but this causes the
    /// excess bytes to be returned instead of dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let mut buf = BytesVec::copy_from_slice(&b"hello world"[..]);
    /// buf.truncate(5);
    /// assert_eq!(buf, b"hello"[..]);
    /// ```
    ///
    /// [`split_off`]: #method.split_off
    pub fn truncate(&mut self, len: usize) {
        self.inner.truncate(len);
    }

    /// Clears the buffer, removing all data.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let mut buf = BytesVec::copy_from_slice(&b"hello world"[..]);
    /// buf.clear();
    /// assert!(buf.is_empty());
    /// ```
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Resizes the buffer so that `len` is equal to `new_len`.
    ///
    /// If `new_len` is greater than `len`, the buffer is extended by the
    /// difference with each additional byte set to `value`. If `new_len` is
    /// less than `len`, the buffer is simply truncated.
    ///
    /// # Panics
    ///
    /// Panics if `new_len` greater than 60bit for 64bit systems
    /// and 28bit for 32bit systems
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let mut buf = BytesVec::new();
    ///
    /// buf.resize(3, 0x1);
    /// assert_eq!(&buf[..], &[0x1, 0x1, 0x1]);
    ///
    /// buf.resize(2, 0x2);
    /// assert_eq!(&buf[..], &[0x1, 0x1]);
    ///
    /// buf.resize(4, 0x3);
    /// assert_eq!(&buf[..], &[0x1, 0x1, 0x3, 0x3]);
    /// ```
    #[inline]
    pub fn resize(&mut self, new_len: usize, value: u8) {
        self.inner.resize(new_len, value);
    }

    /// Sets the length of the buffer.
    ///
    /// This will explicitly set the size of the buffer without actually
    /// modifying the data, so it is up to the caller to ensure that the data
    /// has been initialized.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let mut b = BytesVec::copy_from_slice(&b"hello world"[..]);
    ///
    /// unsafe {
    ///     b.set_len(5);
    /// }
    ///
    /// assert_eq!(&b[..], b"hello");
    ///
    /// unsafe {
    ///     b.set_len(11);
    /// }
    ///
    /// assert_eq!(&b[..], b"hello world");
    /// ```
    ///
    /// # Panics
    ///
    /// This method will panic if `len` is out of bounds for the underlying
    /// slice or if it comes after the `end` of the configured window.
    #[inline]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn set_len(&mut self, len: usize) {
        self.inner.set_len(len)
    }

    /// Reserves capacity for at least `additional` more bytes to be inserted
    /// into the given `BytesVec`.
    ///
    /// More than `additional` bytes may be reserved in order to avoid frequent
    /// reallocations. A call to `reserve` may result in an allocation.
    ///
    /// Before allocating new buffer space, the function will attempt to reclaim
    /// space in the existing buffer. If the current handle references a small
    /// view in the original buffer and all other handles have been dropped,
    /// and the requested capacity is less than or equal to the existing
    /// buffer's capacity, then the current view will be copied to the front of
    /// the buffer and the handle will take ownership of the full buffer.
    ///
    /// # Panics
    ///
    /// Panics if new capacity is greater than 60bit for 64bit systems
    /// and 28bit for 32bit systems
    ///
    /// # Examples
    ///
    /// In the following example, a new buffer is allocated.
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let mut buf = BytesVec::copy_from_slice(&b"hello"[..]);
    /// buf.reserve(64);
    /// assert!(buf.capacity() >= 69);
    /// ```
    ///
    /// In the following example, the existing buffer is reclaimed.
    ///
    /// ```
    /// use ntex_bytes::{BytesVec, BufMut};
    ///
    /// let mut buf = BytesVec::with_capacity(128);
    /// buf.put(&[0; 64][..]);
    ///
    /// let ptr = buf.as_ptr();
    /// let other = buf.split();
    ///
    /// assert!(buf.is_empty());
    /// assert_eq!(buf.capacity(), 64);
    ///
    /// drop(other);
    /// buf.reserve(128);
    ///
    /// assert_eq!(buf.capacity(), 128);
    /// assert_eq!(buf.as_ptr(), ptr);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if the new capacity overflows `usize`.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        let len = self.len();
        let rem = self.capacity() - len;

        if additional <= rem {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.inner.reserve_inner(additional);
    }

    /// Appends given bytes to this object.
    ///
    /// If this `BytesVec` object has not enough capacity, it is resized first.
    /// So unlike `put_slice` operation, `extend_from_slice` does not panic.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesVec;
    ///
    /// let mut buf = BytesVec::with_capacity(0);
    /// buf.extend_from_slice(b"aaabbb");
    /// buf.extend_from_slice(b"cccddd");
    ///
    /// assert_eq!(b"aaabbbcccddd", &buf[..]);
    /// ```
    #[inline]
    pub fn extend_from_slice(&mut self, extend: &[u8]) {
        self.put_slice(extend);
    }

    /// Run provided function with `BytesMut` instance that contains current data.
    #[inline]
    pub fn with_bytes_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        self.inner.with_bytes_mut(f)
    }

    /// Returns an iterator over the bytes contained by the buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{Buf, BytesVec};
    ///
    /// let buf = BytesVec::copy_from_slice(&b"abc"[..]);
    /// let mut iter = buf.iter();
    ///
    /// assert_eq!(iter.next().map(|b| *b), Some(b'a'));
    /// assert_eq!(iter.next().map(|b| *b), Some(b'b'));
    /// assert_eq!(iter.next().map(|b| *b), Some(b'c'));
    /// assert_eq!(iter.next(), None);
    /// ```
    #[inline]
    pub fn iter(&'_ self) -> std::slice::Iter<'_, u8> {
        self.chunk().iter()
    }

    pub(crate) fn move_to_pool(&mut self, pool: PoolRef) {
        self.inner.move_to_pool(pool);
    }
}

impl Buf for BytesVec {
    #[inline]
    fn remaining(&self) -> usize {
        self.len()
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        self.inner.as_ref()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        assert!(
            cnt <= self.inner.as_ref().len(),
            "cannot advance past `remaining`"
        );
        unsafe {
            self.inner.set_start(cnt as u32);
        }
    }
}

impl BufMut for BytesVec {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.capacity() - self.len()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        let new_len = self.len() + cnt;

        // This call will panic if `cnt` is too big
        self.inner.set_len(new_len);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        let len = self.len();

        unsafe {
            // This will never panic as `len` can never become invalid
            let ptr = &mut self.inner.as_raw()[len..];

            UninitSlice::from_raw_parts_mut(ptr.as_mut_ptr(), self.capacity() - len)
        }
    }

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        let len = src.len();
        self.reserve(len);

        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), self.chunk_mut().as_mut_ptr(), len);
            self.advance_mut(len);
        }
    }

    #[inline]
    fn put_u8(&mut self, n: u8) {
        self.reserve(1);
        self.inner.put_u8(n);
    }

    #[inline]
    fn put_i8(&mut self, n: i8) {
        self.reserve(1);
        self.put_u8(n as u8);
    }
}

impl AsRef<[u8]> for BytesVec {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl AsMut<[u8]> for BytesVec {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.inner.as_mut()
    }
}

impl Deref for BytesVec {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl DerefMut for BytesVec {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.inner.as_mut()
    }
}

impl Eq for BytesVec {}

impl PartialEq for BytesVec {
    #[inline]
    fn eq(&self, other: &BytesVec) -> bool {
        self.inner.as_ref() == other.inner.as_ref()
    }
}

impl Default for BytesVec {
    #[inline]
    fn default() -> BytesVec {
        BytesVec::new()
    }
}

impl Borrow<[u8]> for BytesVec {
    #[inline]
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl BorrowMut<[u8]> for BytesVec {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8] {
        self.as_mut()
    }
}

impl fmt::Debug for BytesVec {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&debug::BsDebug(self.inner.as_ref()), fmt)
    }
}

impl fmt::Write for BytesVec {
    #[inline]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.remaining_mut() >= s.len() {
            self.put_slice(s.as_bytes());
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }

    #[inline]
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        fmt::write(self, args)
    }
}

impl IntoIterator for BytesVec {
    type Item = u8;
    type IntoIter = IntoIter<BytesVec>;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter::new(self)
    }
}

impl<'a> IntoIterator for &'a BytesVec {
    type Item = &'a u8;
    type IntoIter = std::slice::Iter<'a, u8>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_ref().iter()
    }
}

impl FromIterator<u8> for BytesVec {
    fn from_iter<T: IntoIterator<Item = u8>>(into_iter: T) -> Self {
        let iter = into_iter.into_iter();
        let (min, maybe_max) = iter.size_hint();

        let mut out = BytesVec::with_capacity(maybe_max.unwrap_or(min));
        for i in iter {
            out.reserve(1);
            out.put_u8(i);
        }

        out
    }
}

impl<'a> FromIterator<&'a u8> for BytesVec {
    fn from_iter<T: IntoIterator<Item = &'a u8>>(into_iter: T) -> Self {
        into_iter.into_iter().copied().collect::<BytesVec>()
    }
}

impl Extend<u8> for BytesVec {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = u8>,
    {
        let iter = iter.into_iter();

        let (lower, _) = iter.size_hint();
        self.reserve(lower);

        for b in iter {
            self.put_u8(b);
        }
    }
}

impl<'a> Extend<&'a u8> for BytesVec {
    fn extend<T>(&mut self, iter: T)
    where
        T: IntoIterator<Item = &'a u8>,
    {
        self.extend(iter.into_iter().copied())
    }
}

struct InnerVec(NonNull<SharedVec>);

impl InnerVec {
    #[inline]
    fn with_capacity(capacity: usize, pool: PoolRef) -> InnerVec {
        Self::from_slice(capacity, &[], pool)
    }

    #[inline]
    fn from_slice(cap: usize, src: &[u8], pool: PoolRef) -> InnerVec {
        // vec must be aligned to SharedVec instead of u8
        let vec_cap = if cap % SHARED_VEC_SIZE != 0 {
            (cap / SHARED_VEC_SIZE) + 2
        } else {
            (cap / SHARED_VEC_SIZE) + 1
        };
        let mut vec = Vec::<SharedVec>::with_capacity(vec_cap);
        unsafe {
            // Store data in vec
            let len = src.len() as u32;
            let cap = vec.capacity() * SHARED_VEC_SIZE;
            let shared_ptr = vec.as_mut_ptr();
            mem::forget(vec);
            pool.acquire(cap);

            let ptr = shared_ptr.add(1) as *mut u8;
            if !src.is_empty() {
                ptr::copy_nonoverlapping(src.as_ptr(), ptr, src.len());
            }
            ptr::write(
                shared_ptr,
                SharedVec {
                    len,
                    cap,
                    pool,
                    ref_count: AtomicUsize::new(1),
                    offset: SHARED_VEC_SIZE as u32,
                },
            );

            InnerVec(NonNull::new_unchecked(shared_ptr))
        }
    }

    #[inline]
    fn move_to_pool(&mut self, pool: PoolRef) {
        unsafe {
            let inner = self.as_inner();
            if pool != inner.pool {
                pool.acquire(inner.cap);
                let pool = mem::replace(&mut inner.pool, pool);
                pool.release(inner.cap);
            }
        }
    }

    /// Return a slice for the handle's view into the shared buffer
    #[inline]
    fn as_ref(&self) -> &[u8] {
        unsafe { slice::from_raw_parts(self.as_ptr(), self.len()) }
    }

    /// Return a mutable slice for the handle's view into the shared buffer
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.as_ptr(), self.len()) }
    }

    /// Return a mutable slice for the handle's view into the shared buffer
    /// including potentially uninitialized bytes.
    #[inline]
    unsafe fn as_raw(&mut self) -> &mut [u8] {
        slice::from_raw_parts_mut(self.as_ptr(), self.capacity())
    }

    /// Return a raw pointer to data
    #[inline]
    unsafe fn as_ptr(&self) -> *mut u8 {
        (self.0.as_ptr() as *mut u8).add((*self.0.as_ptr()).offset as usize)
    }

    #[inline]
    unsafe fn as_inner(&mut self) -> &mut SharedVec {
        self.0.as_mut()
    }

    /// Insert a byte into the next slot and advance the len by 1.
    #[inline]
    fn put_u8(&mut self, n: u8) {
        unsafe {
            let inner = self.as_inner();
            let len = inner.len as usize;
            assert!(len < (inner.cap - inner.offset as usize));
            inner.len += 1;
            *self.as_ptr().add(len) = n;
        }
    }

    #[inline]
    fn len(&self) -> usize {
        unsafe { (*self.0.as_ptr()).len as usize }
    }

    /// slice.
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        let inner = self.as_inner();
        assert!(len <= (inner.cap - inner.offset as usize) && len < u32::MAX as usize);
        inner.len = len as u32;
    }

    #[inline]
    fn capacity(&self) -> usize {
        unsafe { (*self.0.as_ptr()).cap - (*self.0.as_ptr()).offset as usize }
    }

    fn into_inner(mut self) -> Inner {
        unsafe {
            let ptr = self.as_ptr();

            if self.len() <= INLINE_CAP {
                Inner::from_ptr_inline(ptr, self.len())
            } else {
                let inner = self.as_inner();

                let inner = Inner {
                    ptr,
                    len: inner.len as usize,
                    cap: inner.cap - inner.offset as usize,
                    arc: NonNull::new_unchecked(
                        (self.0.as_ptr() as usize ^ KIND_VEC) as *mut Shared,
                    ),
                };
                mem::forget(self);
                inner
            }
        }
    }

    fn with_bytes_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        unsafe {
            // create Inner for BytesMut
            let ptr = self.as_ptr();
            let inner = self.as_inner();
            let inner = Inner {
                ptr,
                len: inner.len as usize,
                cap: inner.cap - inner.offset as usize,
                arc: NonNull::new_unchecked(
                    (self.0.as_ptr() as usize ^ KIND_VEC) as *mut Shared,
                ),
            };

            // run function
            let mut buf = BytesMut { inner };
            let result = f(&mut buf);

            // convert BytesMut back to InnerVec
            let kind = buf.inner.kind();
            let new_inner =
                // only KIND_VEC could be converted to self, otherwise we have to copy data
                if kind == KIND_INLINE || kind == KIND_STATIC || kind == KIND_ARC {
                    InnerVec::from_slice(
                        buf.inner.capacity(),
                        buf.inner.as_ref(),
                        buf.inner.pool(),
                    )
                } else if kind == KIND_VEC {
                    let ptr = buf.inner.shared_vec();
                    let offset = buf.inner.ptr as usize - ptr as usize;

                    // we cannot use shared vec if BytesMut points to inside of vec
                    if buf.inner.cap < (*ptr).cap - offset {
                        InnerVec::from_slice(
                            buf.inner.capacity(),
                            buf.inner.as_ref(),
                            buf.inner.pool(),
                        )
                    } else {
                        // BytesMut owns rest of the vec, so re-use
                        (*ptr).len = buf.len() as u32;
                        (*ptr).offset = offset as u32;
                        let inner = InnerVec(NonNull::new_unchecked(ptr));
                        mem::forget(buf); // reuse bytes
                        inner
                    }
                } else {
                    panic!()
                };

            // drop old inner, we cannot drop because BytesMut used it
            let old = mem::replace(self, new_inner);
            mem::forget(old);

            result
        }
    }

    fn split_to(&mut self, at: usize, create_inline: bool) -> Inner {
        unsafe {
            let ptr = self.as_ptr();

            let other = if create_inline && at <= INLINE_CAP {
                Inner::from_ptr_inline(ptr, at)
            } else {
                let inner = self.as_inner();
                let old_size = inner.ref_count.fetch_add(1, Relaxed);
                if old_size == usize::MAX {
                    abort();
                }

                Inner {
                    ptr,
                    len: at,
                    cap: at,
                    arc: NonNull::new_unchecked(
                        (self.0.as_ptr() as usize ^ KIND_VEC) as *mut Shared,
                    ),
                }
            };
            self.set_start(at as u32);

            other
        }
    }

    fn truncate(&mut self, len: usize) {
        unsafe {
            if len <= self.len() {
                self.set_len(len);
            }
        }
    }

    fn resize(&mut self, new_len: usize, value: u8) {
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
    fn reserve(&mut self, additional: usize) {
        let len = self.len();
        let rem = self.capacity() - len;

        if additional <= rem {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.reserve_inner(additional)
    }

    #[inline]
    // In separate function to allow the short-circuits in `reserve` to
    // be inline-able. Significant helps performance.
    fn reserve_inner(&mut self, additional: usize) {
        let len = self.len();

        // Reserving involves abandoning the currently shared buffer and
        // allocating a new vector with the requested capacity.
        let new_cap = len + additional;

        unsafe {
            let inner = self.as_inner();
            let vec_cap = inner.cap - SHARED_VEC_SIZE;

            // try to reclaim the buffer. This is possible if the current
            // handle is the only outstanding handle pointing to the buffer.
            if inner.is_unique() && vec_cap >= new_cap {
                let offset = inner.offset;
                inner.offset = SHARED_VEC_SIZE as u32;

                // The capacity is sufficient, reclaim the buffer
                let src = (self.0.as_ptr() as *mut u8).add(offset as usize);
                let dst = (self.0.as_ptr() as *mut u8).add(SHARED_VEC_SIZE);
                ptr::copy(src, dst, len);
            } else {
                // Create a new vector storage
                let pool = inner.pool;
                *self = InnerVec::from_slice(new_cap, self.as_ref(), pool);
            }
        }
    }

    unsafe fn set_start(&mut self, start: u32) {
        // Setting the start to 0 is a no-op, so return early if this is the
        // case.
        if start == 0 {
            return;
        }

        let inner = self.as_inner();
        assert!(start <= inner.cap as u32);

        // Updating the start of the view is setting `offset` to point to the
        // new start and updating the `len` field to reflect the new length
        // of the view.
        inner.offset += start;

        if inner.len >= start {
            inner.len -= start;
        } else {
            inner.len = 0;
        }
    }
}

impl Drop for InnerVec {
    fn drop(&mut self) {
        release_shared_vec(self.0.as_ptr());
    }
}

/*
 *
 * ===== Inner =====
 *
 */

impl Inner {
    #[inline]
    const fn from_static(bytes: &'static [u8]) -> Inner {
        let ptr = bytes.as_ptr() as *mut u8;

        Inner {
            // `arc` won't ever store a pointer. Instead, use it to
            // track the fact that the `Bytes` handle is backed by a
            // static buffer.
            arc: unsafe { NonNull::new_unchecked(KIND_STATIC as *mut Shared) },
            ptr,
            len: bytes.len(),
            cap: bytes.len(),
        }
    }

    #[inline]
    const fn empty_inline() -> Inner {
        Inner {
            arc: unsafe { NonNull::new_unchecked(KIND_INLINE as *mut Shared) },
            ptr: 0 as *mut u8,
            len: 0,
            cap: 0,
        }
    }

    #[inline]
    fn from_vec(mut vec: Vec<u8>, pool: PoolRef) -> Inner {
        let len = vec.len();
        let cap = vec.capacity();
        let ptr = vec.as_mut_ptr();
        pool.acquire(cap);

        // Store data in arc
        let shared = Box::into_raw(Box::new(Shared {
            vec,
            pool,
            ref_count: AtomicUsize::new(1),
        }));

        // The pointer should be aligned, so this assert should always succeed.
        debug_assert!(0 == (shared as usize & KIND_MASK));

        // Create new arc, so atomic operations can be avoided.
        Inner {
            ptr,
            len,
            cap,
            arc: unsafe { NonNull::new_unchecked(shared) },
        }
    }

    #[inline]
    fn with_capacity(capacity: usize, pool: PoolRef) -> Inner {
        Inner::from_slice(capacity, &[], pool)
    }

    #[inline]
    fn from_slice(cap: usize, src: &[u8], pool: PoolRef) -> Inner {
        // vec must be aligned to SharedVec instead of u8
        let mut vec_cap = (cap / SHARED_VEC_SIZE) + 1;
        if cap % SHARED_VEC_SIZE != 0 {
            vec_cap += 1;
        }
        let mut vec = Vec::<SharedVec>::with_capacity(vec_cap);

        // Store data in vec
        let len = src.len();
        let full_cap = vec.capacity() * SHARED_VEC_SIZE;
        let cap = full_cap - SHARED_VEC_SIZE;
        vec.push(SharedVec {
            pool,
            cap: full_cap,
            ref_count: AtomicUsize::new(1),
            len: 0,
            offset: 0,
        });
        pool.acquire(full_cap);

        let shared_ptr = vec.as_mut_ptr();
        mem::forget(vec);

        let (ptr, arc) = unsafe {
            let ptr = shared_ptr.add(1) as *mut u8;
            ptr::copy_nonoverlapping(src.as_ptr(), ptr, src.len());
            let arc =
                NonNull::new_unchecked((shared_ptr as usize ^ KIND_VEC) as *mut Shared);
            (ptr, arc)
        };

        // Create new arc, so atomic operations can be avoided.
        Inner { len, cap, ptr, arc }
    }

    #[inline]
    fn from_slice_inline(src: &[u8]) -> Inner {
        unsafe { Inner::from_ptr_inline(src.as_ptr(), src.len()) }
    }

    #[inline]
    unsafe fn from_ptr_inline(src: *const u8, len: usize) -> Inner {
        let mut inner = Inner {
            arc: NonNull::new_unchecked(KIND_INLINE as *mut Shared),
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
        };

        let dst = inner.inline_ptr();
        ptr::copy(src, dst, len);
        inner.set_inline_len(len);
        inner
    }

    #[inline]
    fn pool(&self) -> PoolRef {
        let kind = self.kind();

        if kind == KIND_VEC {
            unsafe { (*self.shared_vec()).pool }
        } else if kind == KIND_ARC {
            unsafe { (*self.arc.as_ptr()).pool }
        } else {
            PoolId::DEFAULT.pool_ref()
        }
    }

    #[inline]
    fn move_to_pool(&mut self, pool: PoolRef) {
        let kind = self.kind();

        if kind == KIND_VEC {
            let vec = self.shared_vec();
            unsafe {
                let cap = (*vec).cap;
                pool.acquire(cap);
                let pool = mem::replace(&mut (*vec).pool, pool);
                pool.release(cap);
            }
        } else if kind == KIND_ARC {
            let arc = self.arc.as_ptr();
            unsafe {
                let cap = (*arc).vec.capacity();
                pool.acquire(cap);
                let pool = mem::replace(&mut (*arc).pool, pool);
                pool.release(cap);
            }
        }
    }

    /// Return a slice for the handle's view into the shared buffer
    #[inline]
    fn as_ref(&self) -> &[u8] {
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
    fn as_mut(&mut self) -> &mut [u8] {
        debug_assert!(!self.is_static());

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
    unsafe fn as_raw(&mut self) -> &mut [u8] {
        debug_assert!(!self.is_static());

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
    fn put_u8(&mut self, n: u8) {
        if self.is_inline() {
            let len = self.inline_len();
            assert!(len < INLINE_CAP);
            unsafe {
                *self.inline_ptr().add(len) = n;
            }
            self.set_inline_len(len + 1);
        } else {
            assert!(self.len < self.cap);
            unsafe {
                *self.ptr.add(self.len) = n;
            }
            self.len += 1;
        }
    }

    #[inline]
    fn len(&self) -> usize {
        if self.is_inline() {
            self.inline_len()
        } else {
            self.len
        }
    }

    /// Pointer to the start of the inline buffer
    #[inline]
    unsafe fn inline_ptr(&mut self) -> *mut u8 {
        (self as *mut Inner as *mut u8).offset(INLINE_DATA_OFFSET)
    }

    /// Pointer to the start of the inline buffer
    #[inline]
    unsafe fn inline_ptr_ro(&self) -> *const u8 {
        (self as *const Inner as *const u8).offset(INLINE_DATA_OFFSET)
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
                ((self.arc.as_ptr() as usize & !INLINE_LEN_MASK)
                    | (len << INLINE_LEN_OFFSET)) as _,
            )
        };
    }

    /// slice.
    #[inline]
    unsafe fn set_len(&mut self, len: usize) {
        if self.is_inline() {
            assert!(len <= INLINE_CAP);
            self.set_inline_len(len);
        } else {
            assert!(len <= self.cap);
            self.len = len;
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn capacity(&self) -> usize {
        if self.is_inline() {
            INLINE_CAP
        } else {
            self.cap
        }
    }

    fn split_off(&mut self, at: usize, create_inline: bool) -> Inner {
        let other = unsafe {
            if create_inline && self.len() - at <= INLINE_CAP {
                Inner::from_ptr_inline(self.as_ptr().add(at), self.len() - at)
            } else {
                let mut other = self.shallow_clone();
                other.set_start(at);
                other
            }
        };
        unsafe {
            if create_inline && at <= INLINE_CAP {
                *self = Inner::from_ptr_inline(self.as_ptr(), at);
            } else {
                self.set_end(at);
            }
        }

        other
    }

    fn split_to(&mut self, at: usize, create_inline: bool) -> Inner {
        let other = unsafe {
            if create_inline && at <= INLINE_CAP {
                Inner::from_ptr_inline(self.as_ptr(), at)
            } else {
                let mut other = self.shallow_clone();
                other.set_end(at);
                other
            }
        };
        unsafe {
            if create_inline && self.len() - at <= INLINE_CAP {
                *self = Inner::from_ptr_inline(self.as_ptr().add(at), self.len() - at);
            } else {
                self.set_start(at);
            }
        }

        other
    }

    fn truncate(&mut self, len: usize, create_inline: bool) {
        unsafe {
            if len <= self.len() {
                if create_inline && len < INLINE_CAP {
                    *self = Inner::from_ptr_inline(self.as_ptr(), len);
                } else {
                    self.set_len(len);
                }
            }
        }
    }

    fn resize(&mut self, new_len: usize, value: u8) {
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

    unsafe fn set_start(&mut self, start: usize) {
        // Setting the start to 0 is a no-op, so return early if this is the
        // case.
        if start == 0 {
            return;
        }

        let kind = self.kind();

        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Inner` struct fields will be gibberish.
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

    unsafe fn set_end(&mut self, end: usize) {
        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Inner` struct fields will be gibberish.
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
    fn is_mut_safe(&self) -> bool {
        let kind = self.kind();

        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Inner` struct fields will be gibberish.
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
    unsafe fn shallow_clone(&self) -> Inner {
        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Inner` struct fields will be gibberish.
        //
        // Additionally, if kind is STATIC, then Arc is *never* changed, making
        // it safe and faster to check for it now before an atomic acquire.

        if self.is_inline_or_static() {
            // In this case, a shallow_clone still involves copying the data.
            let mut inner: mem::MaybeUninit<Inner> = mem::MaybeUninit::uninit();
            ptr::copy_nonoverlapping(self, inner.as_mut_ptr(), 1);
            inner.assume_init()
        } else {
            self.shallow_clone_sync()
        }
    }

    #[cold]
    unsafe fn shallow_clone_sync(&self) -> Inner {
        // The function requires `&self`, this means that `shallow_clone`
        // could be called concurrently.
        //
        // The first step is to load the value of `arc`. This will determine
        // how to proceed. The `Acquire` ordering synchronizes with the
        // `compare_and_swap` that comes later in this function. The goal is
        // to ensure that if `arc` is currently set to point to a `Shared`,
        // that the current thread acquires the associated memory.
        let arc: *mut Shared = self.arc.as_ptr();
        let kind = arc as usize & KIND_MASK;

        if kind == KIND_ARC {
            let old_size = (*arc).ref_count.fetch_add(1, Relaxed);
            if old_size == usize::MAX {
                abort();
            }

            Inner {
                arc: NonNull::new_unchecked(arc),
                ..*self
            }
        } else {
            assert!(kind == KIND_VEC);

            let vec_arc = (arc as usize & KIND_UNMASK) as *mut SharedVec;
            let old_size = (*vec_arc).ref_count.fetch_add(1, Relaxed);
            if old_size == usize::MAX {
                abort();
            }

            Inner {
                arc: NonNull::new_unchecked(arc),
                ..*self
            }
        }
    }

    #[inline]
    fn reserve(&mut self, additional: usize) {
        let len = self.len();
        let rem = self.capacity() - len;

        if additional <= rem {
            // The handle can already store at least `additional` more bytes, so
            // there is no further work needed to be done.
            return;
        }

        self.reserve_inner(additional)
    }

    #[inline]
    // In separate function to allow the short-circuits in `reserve` to
    // be inline-able. Significant helps performance.
    fn reserve_inner(&mut self, additional: usize) {
        let len = self.len();
        let kind = self.kind();

        // Always check `inline` first, because if the handle is using inline
        // data storage, all of the `Inner` struct fields will be gibberish.
        if kind == KIND_INLINE {
            let new_cap = len + additional;

            // Promote to a vector
            *self = Inner::from_slice(new_cap, self.as_ref(), PoolId::DEFAULT.pool_ref());
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
                if (*vec).is_unique() && vec_cap >= new_cap {
                    // The capacity is sufficient, reclaim the buffer
                    let ptr = (vec as *mut u8).add(SHARED_VEC_SIZE);
                    ptr::copy(self.ptr, ptr, len);

                    self.ptr = ptr;
                    self.cap = vec_cap;
                } else {
                    // Create a new vector storage
                    *self = Inner::from_slice(new_cap, self.as_ref(), (*vec).pool);
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
                *self = Inner::from_slice(new_cap, self.as_ref(), (*arc).pool);
            }
        }
    }

    /// Returns true if the buffer is stored inline
    #[inline]
    fn is_inline(&self) -> bool {
        self.kind() == KIND_INLINE
    }

    #[inline]
    fn is_inline_or_static(&self) -> bool {
        // The value returned by `kind` isn't itself safe, but the value could
        // inform what operations to take, and unsafely do something without
        // synchronization.
        //
        // KIND_INLINE and KIND_STATIC will *never* change, so branches on that
        // information is safe.
        let kind = self.kind();
        kind == KIND_INLINE || kind == KIND_STATIC
    }

    /// Used for `debug_assert` statements
    #[inline]
    fn is_static(&self) -> bool {
        matches!(self.kind(), KIND_STATIC)
    }

    #[inline]
    fn shared_vec(&self) -> *mut SharedVec {
        ((self.arc.as_ptr() as usize) & KIND_UNMASK) as *mut SharedVec
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
        // this bit is set when `Inner` is created and never changed after that.
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
}

impl Drop for Inner {
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
        let arc = Box::from_raw(ptr);
        arc.pool.release(arc.vec.capacity());
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

        // Drop vec
        let cap = (*ptr).cap;
        (*ptr).pool.release(cap);

        Vec::<SharedVec>::from_raw_parts(ptr, 1, cap / SHARED_VEC_SIZE);
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
    fn is_unique(&self) -> bool {
        // This is same as Shared::is_unique() but for KIND_VEC
        self.ref_count.load(Acquire) == 1
    }
}

unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

/*
 *
 * ===== PartialEq / PartialOrd =====
 *
 */

impl PartialEq<[u8]> for BytesMut {
    fn eq(&self, other: &[u8]) -> bool {
        &**self == other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for BytesMut {
    fn eq(&self, other: &[u8; N]) -> bool {
        &**self == other
    }
}

impl PartialEq<BytesMut> for [u8] {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl<const N: usize> PartialEq<BytesMut> for [u8; N] {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl PartialEq<str> for BytesMut {
    fn eq(&self, other: &str) -> bool {
        &**self == other.as_bytes()
    }
}

impl PartialEq<BytesMut> for str {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl PartialEq<Vec<u8>> for BytesMut {
    fn eq(&self, other: &Vec<u8>) -> bool {
        *self == other[..]
    }
}

impl PartialEq<BytesMut> for Vec<u8> {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl PartialEq<String> for BytesMut {
    fn eq(&self, other: &String) -> bool {
        *self == other[..]
    }
}

impl PartialEq<BytesMut> for String {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl<'a, T: ?Sized> PartialEq<&'a T> for BytesMut
where
    BytesMut: PartialEq<T>,
{
    fn eq(&self, other: &&'a T) -> bool {
        *self == **other
    }
}

impl PartialEq<BytesMut> for &[u8] {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl PartialEq<BytesMut> for &str {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl PartialEq<[u8]> for Bytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.inner.as_ref() == other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for Bytes {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.inner.as_ref() == other.as_ref()
    }
}

impl PartialOrd<[u8]> for Bytes {
    fn partial_cmp(&self, other: &[u8]) -> Option<cmp::Ordering> {
        self.inner.as_ref().partial_cmp(other)
    }
}

impl<const N: usize> PartialOrd<[u8; N]> for Bytes {
    fn partial_cmp(&self, other: &[u8; N]) -> Option<cmp::Ordering> {
        self.inner.as_ref().partial_cmp(other.as_ref())
    }
}

impl PartialEq<Bytes> for [u8] {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl<const N: usize> PartialEq<Bytes> for [u8; N] {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialOrd<Bytes> for [u8] {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        other.partial_cmp(self)
    }
}

impl<const N: usize> PartialOrd<Bytes> for [u8; N] {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        other.partial_cmp(self)
    }
}

impl PartialEq<str> for Bytes {
    fn eq(&self, other: &str) -> bool {
        self.inner.as_ref() == other.as_bytes()
    }
}

impl PartialOrd<str> for Bytes {
    fn partial_cmp(&self, other: &str) -> Option<cmp::Ordering> {
        self.inner.as_ref().partial_cmp(other.as_bytes())
    }
}

impl PartialEq<Bytes> for str {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialOrd<Bytes> for str {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        other.partial_cmp(self)
    }
}

impl PartialEq<Vec<u8>> for Bytes {
    fn eq(&self, other: &Vec<u8>) -> bool {
        *self == other[..]
    }
}

impl PartialOrd<Vec<u8>> for Bytes {
    fn partial_cmp(&self, other: &Vec<u8>) -> Option<cmp::Ordering> {
        self.inner.as_ref().partial_cmp(&other[..])
    }
}

impl PartialEq<Bytes> for Vec<u8> {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialOrd<Bytes> for Vec<u8> {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        other.partial_cmp(self)
    }
}

impl PartialEq<String> for Bytes {
    fn eq(&self, other: &String) -> bool {
        *self == other[..]
    }
}

impl PartialOrd<String> for Bytes {
    fn partial_cmp(&self, other: &String) -> Option<cmp::Ordering> {
        self.inner.as_ref().partial_cmp(other.as_bytes())
    }
}

impl PartialEq<Bytes> for String {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialOrd<Bytes> for String {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        other.partial_cmp(self)
    }
}

impl PartialEq<Bytes> for &[u8] {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialOrd<Bytes> for &[u8] {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        other.partial_cmp(self)
    }
}

impl PartialEq<Bytes> for &str {
    fn eq(&self, other: &Bytes) -> bool {
        *other == *self
    }
}

impl PartialOrd<Bytes> for &str {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        other.partial_cmp(self)
    }
}

impl<'a, T: ?Sized> PartialEq<&'a T> for Bytes
where
    Bytes: PartialEq<T>,
{
    fn eq(&self, other: &&'a T) -> bool {
        *self == **other
    }
}

impl From<BytesVec> for Bytes {
    fn from(b: BytesVec) -> Self {
        b.freeze()
    }
}

impl<'a, T: ?Sized> PartialOrd<&'a T> for Bytes
where
    Bytes: PartialOrd<T>,
{
    fn partial_cmp(&self, other: &&'a T) -> Option<cmp::Ordering> {
        self.partial_cmp(&**other)
    }
}

impl PartialEq<BytesMut> for Bytes {
    fn eq(&self, other: &BytesMut) -> bool {
        other[..] == self[..]
    }
}

impl PartialEq<BytesVec> for Bytes {
    fn eq(&self, other: &BytesVec) -> bool {
        other[..] == self[..]
    }
}

impl PartialEq<Bytes> for BytesVec {
    fn eq(&self, other: &Bytes) -> bool {
        other[..] == self[..]
    }
}

impl PartialEq<Bytes> for BytesMut {
    fn eq(&self, other: &Bytes) -> bool {
        other[..] == self[..]
    }
}

impl PartialEq<BytesMut> for BytesVec {
    fn eq(&self, other: &BytesMut) -> bool {
        other[..] == self[..]
    }
}

impl PartialEq<BytesVec> for BytesMut {
    fn eq(&self, other: &BytesVec) -> bool {
        other[..] == self[..]
    }
}

impl PartialEq<[u8]> for BytesVec {
    fn eq(&self, other: &[u8]) -> bool {
        &**self == other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for BytesVec {
    fn eq(&self, other: &[u8; N]) -> bool {
        &**self == other
    }
}

impl PartialEq<BytesVec> for [u8] {
    fn eq(&self, other: &BytesVec) -> bool {
        *other == *self
    }
}

impl<const N: usize> PartialEq<BytesVec> for [u8; N] {
    fn eq(&self, other: &BytesVec) -> bool {
        *other == *self
    }
}

impl PartialEq<str> for BytesVec {
    fn eq(&self, other: &str) -> bool {
        &**self == other.as_bytes()
    }
}

impl PartialEq<BytesVec> for str {
    fn eq(&self, other: &BytesVec) -> bool {
        *other == *self
    }
}

impl PartialEq<Vec<u8>> for BytesVec {
    fn eq(&self, other: &Vec<u8>) -> bool {
        *self == other[..]
    }
}

impl PartialEq<BytesVec> for Vec<u8> {
    fn eq(&self, other: &BytesVec) -> bool {
        *other == *self
    }
}

impl PartialEq<String> for BytesVec {
    fn eq(&self, other: &String) -> bool {
        *self == other[..]
    }
}

impl PartialEq<BytesVec> for String {
    fn eq(&self, other: &BytesVec) -> bool {
        *other == *self
    }
}

impl<'a, T: ?Sized> PartialEq<&'a T> for BytesVec
where
    BytesVec: PartialEq<T>,
{
    fn eq(&self, other: &&'a T) -> bool {
        *self == **other
    }
}

impl PartialEq<BytesVec> for &[u8] {
    fn eq(&self, other: &BytesVec) -> bool {
        *other == *self
    }
}

impl PartialEq<BytesVec> for &str {
    fn eq(&self, other: &BytesVec) -> bool {
        *other == *self
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
fn abort() {
    let _a = Abort;
    panic!();
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const LONG: &[u8] = b"mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb";

    #[test]
    fn trimdown() {
        let mut b = Bytes::from(LONG.to_vec());
        assert_eq!(b.inner.capacity(), 263);
        unsafe { b.inner.set_len(68) };
        assert_eq!(b.len(), 68);
        assert_eq!(b.inner.capacity(), 263);
        b.trimdown();
        assert_eq!(b.inner.capacity(), 96);

        unsafe { b.inner.set_len(16) };
        b.trimdown();
        assert!(b.is_inline());
    }

    #[test]
    #[allow(clippy::len_zero)]
    fn bytes() {
        let mut b = Bytes::from(LONG.to_vec());
        b.clear();
        assert!(b.is_inline());
        assert!(b.is_empty());
        assert!(b.len() == 0);

        let b = Bytes::from(&Bytes::from(LONG));
        assert_eq!(b, LONG);

        let b = Bytes::from(BytesMut::from(LONG));
        assert_eq!(b, LONG);

        let mut b: Bytes = BytesMut::try_from(b).unwrap().freeze();
        assert_eq!(b, LONG);
        assert!(!(b > b));
        assert_eq!(<Bytes as Buf>::remaining(&b), LONG.len());
        assert_eq!(<Bytes as Buf>::chunk(&b), LONG);
        <Bytes as Buf>::advance(&mut b, 10);
        assert_eq!(Buf::chunk(&b), &LONG[10..]);

        let mut h: HashMap<Bytes, usize> = HashMap::default();
        h.insert(b.clone(), 1);
        assert_eq!(h.get(&b), Some(&1));

        let mut b = BytesMut::try_from(LONG).unwrap();
        assert_eq!(b, LONG);
        assert_eq!(<BytesMut as Buf>::remaining(&b), LONG.len());
        assert_eq!(<BytesMut as BufMut>::remaining_mut(&b), 25);
        assert_eq!(<BytesMut as Buf>::chunk(&b), LONG);
        <BytesMut as Buf>::advance(&mut b, 10);
        assert_eq!(<BytesMut as Buf>::chunk(&b), &LONG[10..]);

        let mut b = BytesMut::with_capacity(12);
        <BytesMut as BufMut>::put_i8(&mut b, 1);
        assert_eq!(b, b"\x01".as_ref());
        <BytesMut as BufMut>::put_u8(&mut b, 2);
        assert_eq!(b, b"\x01\x02".as_ref());
        <BytesMut as BufMut>::put_slice(&mut b, b"12345");
        assert_eq!(b, b"\x01\x0212345".as_ref());
        <BytesMut as BufMut>::chunk_mut(&mut b).write_byte(0, b'1');
        unsafe { <BytesMut as BufMut>::advance_mut(&mut b, 1) };
        assert_eq!(b, b"\x01\x02123451".as_ref());
    }

    #[test]
    fn bytes_vec() {
        let bv = BytesVec::copy_from_slice(LONG);
        // SharedVec size is 32
        assert_eq!(bv.capacity(), mem::size_of::<SharedVec>() * 9);
        assert_eq!(bv.len(), 263);
        assert_eq!(bv.as_ref().len(), 263);
        assert_eq!(bv.as_ref(), LONG);

        let mut bv = BytesVec::copy_from_slice(&b"hello"[..]);
        assert_eq!(bv.capacity(), mem::size_of::<SharedVec>());
        assert_eq!(bv.len(), 5);
        assert_eq!(bv.as_ref().len(), 5);
        assert_eq!(bv.as_ref()[0], b"h"[0]);
        bv.put_u8(b" "[0]);
        assert_eq!(bv.as_ref(), &b"hello "[..]);
        bv.put("world");
        assert_eq!(bv, "hello world");

        let b = Bytes::from(bv);
        assert_eq!(b, "hello world");

        let mut b = BytesMut::try_from(b).unwrap();
        b.put(".");
        assert_eq!(b, "hello world.");
    }
}
