use std::borrow::{Borrow, BorrowMut};
use std::ops::{Deref, DerefMut, RangeBounds};
use std::{cmp, fmt, hash, mem, ptr};

use crate::storage::{Storage, INLINE_CAP};
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
    pub(crate) inner: Storage,
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
    pub(crate) inner: Storage,
}

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
            inner: Storage::empty(),
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
            inner: Storage::from_static(bytes),
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
        Bytes {
            inner: Storage::from_slice(data),
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
        self.slice_checked(range)
            .expect("Requires that `begin <= end` and `end <= self.len()`")
    }

    /// Returns a slice of self for the provided range.
    ///
    /// Does nothing if `begin <= end` or `end <= self.len()`
    pub fn slice_checked(&self, range: impl RangeBounds<usize>) -> Option<Bytes> {
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

        if begin <= end && end <= len {
            if end - begin <= INLINE_CAP {
                Some(Bytes {
                    inner: Storage::from_slice(&self[begin..end]),
                })
            } else {
                let mut ret = self.clone();
                unsafe {
                    ret.inner.set_end(end);
                    ret.inner.set_start(begin);
                }
                Some(ret)
            }
        } else {
            None
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
        self.slice_ref_checked(subset)
            .expect("Given `sub` slice is not contained within the `Bytes` buffer")
    }

    /// Returns a slice of self that is equivalent to the given `subset`.
    pub fn slice_ref_checked(&self, subset: &[u8]) -> Option<Bytes> {
        let bytes_p = self.as_ptr() as usize;
        let bytes_len = self.len();

        let sub_p = subset.as_ptr() as usize;
        let sub_len = subset.len();

        if sub_p >= bytes_p && sub_p + sub_len <= bytes_p + bytes_len {
            let sub_offset = sub_p - bytes_p;
            Some(self.slice(sub_offset..(sub_offset + sub_len)))
        } else {
            None
        }
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
    /// Panics if `at > self.len()`.
    pub fn split_off(&mut self, at: usize) -> Bytes {
        self.split_off_checked(at)
            .expect("at value must be <= self.len()`")
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Does nothing if `at > self.len()`
    pub fn split_off_checked(&mut self, at: usize) -> Option<Bytes> {
        if at <= self.len() {
            if at == self.len() {
                Some(Bytes::new())
            } else if at == 0 {
                Some(mem::take(self))
            } else {
                Some(Bytes {
                    inner: self.inner.split_off(at, true),
                })
            }
        } else {
            None
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
        self.split_to_checked(at)
            .expect("at value must be <= self.len()`")
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Does nothing if `at > len`.
    pub fn split_to_checked(&mut self, at: usize) -> Option<Bytes> {
        if at <= self.len() {
            if at == self.len() {
                Some(mem::take(self))
            } else if at == 0 {
                Some(Bytes::new())
            } else {
                Some(Bytes {
                    inner: self.inner.split_to(at, true),
                })
            }
        } else {
            None
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
        self.inner.trimdown();
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
        self.inner = Storage::empty();
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
            inner: self.inner.clone(),
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
    fn from(src: Vec<u8>) -> Bytes {
        if src.len() <= INLINE_CAP {
            Bytes {
                inner: Storage::from_slice(&src),
            }
        } else {
            Bytes {
                inner: Storage::from_vec(src),
            }
        }
    }
}

impl From<String> for Bytes {
    fn from(src: String) -> Bytes {
        if src.len() <= INLINE_CAP {
            Bytes {
                inner: Storage::from_slice(src.as_bytes()),
            }
        } else {
            Bytes {
                inner: Storage::from_vec(src.into_bytes()),
            }
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
        BytesMut {
            inner: Storage::with_capacity(capacity),
        }
    }

    /// Creates a new `BytesMut` from slice, by copying it.
    pub fn copy_from_slice<T: AsRef<[u8]>>(src: T) -> Self {
        BytesMut {
            inner: Storage::from_slice(src.as_ref()),
        }
    }

    #[inline]
    /// Convert a `Vec` into a `BytesMut`
    pub fn from_vec(src: Vec<u8>) -> BytesMut {
        BytesMut {
            inner: Storage::from_vec(src),
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
        BytesMut {
            inner: Storage::empty_inline(),
        }
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
        Bytes {
            inner: self.inner.freeze(),
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
        self.split_to(self.len())
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
        self.split_to_checked(at)
            .expect("at value must be <= self.len()`")
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Does nothing if `at > len`.
    pub fn split_to_checked(&mut self, at: usize) -> Option<BytesMut> {
        if at <= self.len() {
            Some(BytesMut {
                inner: self.inner.split_to(at, false),
            })
        } else {
            None
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
        self.inner.reserve(additional);
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

    pub fn is_inline(&self) -> bool {
        self.inner.is_inline()
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
        BytesMut::from_vec(src)
    }
}

impl From<String> for BytesMut {
    #[inline]
    fn from(src: String) -> BytesMut {
        BytesMut::from_vec(src.into_bytes())
    }
}

impl From<&BytesMut> for BytesMut {
    #[inline]
    fn from(src: &BytesMut) -> BytesMut {
        src.clone()
    }
}

impl<'a> From<&'a [u8]> for BytesMut {
    fn from(src: &'a [u8]) -> BytesMut {
        if src.is_empty() {
            BytesMut::new()
        } else {
            BytesMut::copy_from_slice(src)
        }
    }
}

impl<const N: usize> From<[u8; N]> for BytesMut {
    fn from(src: [u8; N]) -> BytesMut {
        BytesMut::copy_from_slice(src)
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for BytesMut {
    fn from(src: &'a [u8; N]) -> BytesMut {
        BytesMut::copy_from_slice(src)
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
            .unwrap_or_else(|src| BytesMut::copy_from_slice(&src[..]))
    }
}

impl From<&Bytes> for BytesMut {
    #[inline]
    fn from(src: &Bytes) -> BytesMut {
        BytesMut::copy_from_slice(&src[..])
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

impl<const N: usize> PartialEq<BytesMut> for &[u8; N] {
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

impl<const N: usize> PartialEq<Bytes> for &[u8; N] {
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

impl PartialEq<Bytes> for BytesMut {
    fn eq(&self, other: &Bytes) -> bool {
        other[..] == self[..]
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const LONG: &[u8] = b"mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb";

    #[test]
    #[allow(
        clippy::len_zero,
        clippy::nonminimal_bool,
        clippy::unnecessary_fallible_conversions
    )]
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
        assert_eq!(<BytesMut as BufMut>::remaining_mut(&b), 1);
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
}
