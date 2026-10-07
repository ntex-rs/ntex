use std::{borrow, fmt, io, ops::DerefMut, ptr};

use crate::{Buf, BufMut, BytePageSize, Bytes, buf::UninitSlice, stvec::StorageVec};

/// A unique reference to a contiguous slice of memory.
///
/// `BytesMut` represents a unique view into a potentially shared memory region.
/// Given the uniqueness guarantee, owners of `BytesMut` handles are able to
/// mutate the memory. It is similar to a `Vec<u8>` but with fewer copies and
/// allocations. It also always allocates.
///
/// For more detail, see [`Bytes`].
///
/// # Growth
///
/// Safe write operations such as [`BufMut::put_slice`], [`BufMut::put_u8`], and
/// [`extend_from_slice`](Self::extend_from_slice) reserve additional capacity
/// when needed. Use [`reserve`](Self::reserve) when the required capacity is
/// known in advance to avoid repeated allocation.
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
pub struct BytesMut {
    pub(crate) storage: StorageVec,
}

impl BytesMut {
    /// Creates a new `BytesMut` with the specified capacity.
    ///
    /// The returned `BytesMut` will be able to hold `capacity` bytes
    /// without reallocating.
    ///
    /// It is important to note that this function does not specify the length
    /// of the returned `BytesMut`, but only the capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` exceeds `u32::MAX` minus the buffer
    /// header size, just under 4 GiB.
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
    #[must_use]
    pub fn with_capacity(capacity: usize) -> BytesMut {
        BytesMut {
            storage: StorageVec::with_capacity(capacity),
        }
    }

    /// Creates a new empty `BytesMut` backed by a page of the specified size.
    ///
    /// The buffer has the [`capacity`](BytePageSize::capacity) of the page
    /// size. Pages are taken from the current thread's page cache, and the
    /// page returns to the cache of the thread that drops the last reference
    /// to it, including [`Bytes`] split off the buffer.
    ///
    /// When the buffer grows, it moves to a page of the size that fits the
    /// new capacity, see [`reserve`](Self::reserve). Above the largest page
    /// size the buffer is a regular allocation without a page size, it is
    /// freed when dropped.
    ///
    /// [`BytePageSize::Unset`] has the allocation size of
    /// [`BytePageSize::Size64`], it creates a `Size64` page.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytePageSize, BytesMut};
    ///
    /// let mut buf = BytesMut::with_page_size(BytePageSize::Size4);
    /// assert_eq!(buf.capacity(), BytePageSize::Size4.capacity());
    /// assert_eq!(buf.page_size(), BytePageSize::Size4);
    ///
    /// buf.extend_from_slice(&[0; 5000]);
    /// assert_eq!(buf.page_size(), BytePageSize::Size8);
    /// ```
    #[inline]
    #[must_use]
    pub fn with_page_size(size: BytePageSize) -> BytesMut {
        let size = if size == BytePageSize::Unset {
            BytePageSize::Size64
        } else {
            size
        };
        BytesMut {
            storage: StorageVec::sized(size),
        }
    }

    /// Returns the page size of the buffer.
    ///
    /// The page size is derived from the allocation size, a buffer whose
    /// allocation, header included, is exactly a page size belongs to that
    /// page size. This covers buffers created by
    /// [`with_page_size`](Self::with_page_size), converted from a pooled
    /// [`BytePage`](crate::BytePage), or created with a page
    /// [`capacity`](BytePageSize::capacity). They return to the page cache
    /// when the last reference is dropped. Other buffers return
    /// [`BytePageSize::Unset`], they are freed when dropped.
    #[inline]
    pub fn page_size(&self) -> BytePageSize {
        self.storage.page_size()
    }

    /// Creates a `BytesMut` by copying a byte slice.
    #[inline]
    #[must_use]
    pub fn copy_from_slice<T: AsRef<[u8]>>(src: T) -> Self {
        let slice = src.as_ref();
        BytesMut {
            storage: StorageVec::from_slice(slice.len(), slice),
        }
    }

    /// Creates a new `BytesMut` with default capacity.
    ///
    /// Resulting object has length 0 and unspecified capacity.
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
    #[must_use]
    pub fn new() -> BytesMut {
        BytesMut {
            storage: StorageVec::with_capacity(crate::storage::MIN_CAPACITY),
        }
    }

    /// Returns the number of bytes contained in this `BytesMut`.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let b = BytesMut::copy_from_slice(&b"hello"[..]);
    /// assert_eq!(b.len(), 5);
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.storage.len()
    }

    /// Returns `true` if the buffer is empty.
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
        self.storage.len() == 0
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
        self.storage.capacity()
    }

    /// Returns `true` if no other handle refers to the underlying buffer.
    ///
    /// Values split off with [`split_to`](Self::split_to) or frozen into
    /// [`Bytes`] share the buffer with `self`. While they exist, clearing
    /// `self` does not reclaim the capacity in front of it, and the whole
    /// allocation stays alive as long as any of them does.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::with_capacity(64);
    /// buf.extend_from_slice(&[0; 32]);
    /// assert!(buf.is_unique());
    ///
    /// let head = buf.split_to(30);
    /// assert!(!buf.is_unique());
    ///
    /// drop(head);
    /// assert!(buf.is_unique());
    /// ```
    #[inline]
    pub fn is_unique(&self) -> bool {
        self.storage.is_unique()
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
    #[must_use]
    pub fn freeze(self) -> Bytes {
        Bytes {
            storage: self.storage.freeze(),
        }
    }

    /// Removes the bytes from the current view, returning them in a
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
    /// use ntex_bytes::{BytesMut, BufMut};
    ///
    /// let mut buf = BytesMut::with_capacity(1024);
    /// buf.put(&b"hello world"[..]);
    ///
    /// let other = buf.take();
    ///
    /// assert!(buf.is_empty());
    /// assert_eq!(1013, buf.capacity());
    ///
    /// assert_eq!(other, b"hello world"[..]);
    /// ```
    #[inline]
    #[must_use]
    pub fn take(&mut self) -> Bytes {
        Bytes {
            storage: self.storage.split_to(self.len()),
        }
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
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut a = BytesMut::copy_from_slice(&b"hello world"[..]);
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
    #[inline]
    #[must_use]
    pub fn split_to(&mut self, at: usize) -> Bytes {
        self.split_to_checked(at)
            .expect("at value must be <= self.len()`")
    }

    /// Advance the internal cursor.
    ///
    /// Afterwards `self` contains elements `[cnt, len)`.
    /// This is an `O(1)` operation.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut a = BytesMut::copy_from_slice(&b"hello world"[..]);
    /// a.advance_to(5);
    ///
    /// a[0] = b'!';
    ///
    /// assert_eq!(&a[..], b"!world");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `cnt > len`.
    #[inline]
    pub fn advance_to(&mut self, cnt: usize) {
        unsafe {
            self.storage.set_start(cnt);
        }
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Returns `None` if `at > len`.
    #[inline]
    #[must_use]
    pub fn split_to_checked(&mut self, at: usize) -> Option<Bytes> {
        if at <= self.len() {
            Some(Bytes {
                storage: self.storage.split_to(at),
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
    /// `truncate(0)` on a buffer that is not shared with any other handle
    /// also reclaims the capacity in front of the current view, see
    /// [`clear`](Self::clear).
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::copy_from_slice(&b"hello world"[..]);
    /// buf.truncate(5);
    /// assert_eq!(buf, b"hello"[..]);
    /// ```
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.storage.truncate(len);
    }

    /// Clears the buffer, removing all data.
    ///
    /// If no other handle refers to the underlying buffer (see
    /// [`is_unique`](Self::is_unique)), the view is reset to the start of the
    /// allocation, so the full capacity becomes available again.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::copy_from_slice(&b"hello world"[..]);
    /// buf.clear();
    /// assert!(buf.is_empty());
    /// ```
    #[inline]
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
    /// Panics if `new_len` exceeds `u32::MAX` minus the buffer
    /// header size, just under 4 GiB.
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
        self.storage.resize(new_len, value);
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
    /// let mut b = BytesMut::copy_from_slice(&b"hello world"[..]);
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
    /// # Safety
    ///
    /// Caller must ensure that data has been initialized.
    ///
    /// # Panics
    ///
    /// Panics if `len > self.capacity()`.
    #[inline]
    pub unsafe fn set_len(&mut self, len: usize) {
        self.storage.set_len(len);
    }

    /// Reserves capacity for at least `additional` more bytes to be inserted
    /// into the given `BytesMut`.
    ///
    /// Before allocating new buffer space, the function will attempt to reclaim
    /// space in the existing buffer. If the current handle references a small
    /// view in the original buffer and all other handles have been dropped,
    /// and the requested capacity is less than or equal to the existing
    /// buffer's capacity, then the current view will be copied to the front of
    /// the buffer and the handle will take ownership of the full buffer.
    ///
    /// Otherwise a unique buffer that is not a pooled page is reallocated,
    /// often in place, and a new buffer is allocated in all other cases. The
    /// new capacity is at least twice the current length, so appending in
    /// small steps reallocates a logarithmic number of times. Use
    /// [`reserve_exact`](Self::reserve_exact) to avoid the doubling.
    ///
    /// A buffer with a [`page_size`](Self::page_size) moves to a page of the
    /// smallest size that fits the new capacity, but not smaller than its
    /// current page size, the old page returns to the page cache. Above the
    /// largest page size, a regular buffer without a page size is allocated.
    ///
    /// # Panics
    ///
    /// Panics if the new capacity exceeds `u32::MAX` minus the buffer
    /// header size, just under 4 GiB.
    ///
    /// # Examples
    ///
    /// In the following example, a new buffer is allocated.
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::copy_from_slice(&b"hello"[..]);
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
    /// let other = buf.take();
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
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.storage.reserve(additional);
    }

    /// Reserves capacity for exactly `additional` more bytes to be inserted
    /// into the given `BytesMut`.
    ///
    /// Behaves like [`reserve`](Self::reserve), it reclaims the existing buffer
    /// when possible and reallocates a unique buffer, but a new allocation is
    /// sized to hold exactly `additional` more bytes instead of growing to at
    /// least twice the current length. The allocation size is the new capacity
    /// plus the buffer header, [`METADATA_SIZE`](crate::METADATA_SIZE) bytes.
    /// The contents are not moved when the buffer already has enough remaining
    /// capacity.
    ///
    /// The capacity is not rounded up to a page size, a buffer with a
    /// [`page_size`](Self::page_size) loses it unless the new capacity is a page
    /// capacity.
    ///
    /// # Panics
    ///
    /// Panics if the new capacity exceeds `u32::MAX` minus the buffer
    /// header size, just under 4 GiB.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::BytesMut;
    ///
    /// let mut buf = BytesMut::copy_from_slice(&[0; 1000][..]);
    /// buf.reserve_exact(24);
    /// assert_eq!(buf.capacity(), 1024);
    ///
    /// // enough remaining capacity keeps the current buffer
    /// let ptr = buf.as_ptr();
    /// buf.reserve_exact(24);
    /// assert_eq!(buf.as_ptr(), ptr);
    /// ```
    #[inline]
    pub fn reserve_exact(&mut self, additional: usize) {
        self.storage.reserve_exact(additional);
    }

    /// Grows the buffer by one step if its remaining capacity is less than
    /// half of its page size.
    ///
    /// Nothing happens when the remaining capacity is at least
    /// [`BytePageSize::half_capacity`](crate::BytePageSize::half_capacity) of
    /// the buffer's [`page_size`](Self::page_size), 16 KiB for a buffer
    /// without a page size. Otherwise a buffer with a page size moves to a
    /// page of the next larger page size, the old page returns to the page
    /// cache. A buffer without a page size, or with the largest page size,
    /// grows its capacity by its current capacity, by at least 112 bytes and
    /// by at most 64 KiB.
    ///
    /// The new capacity is reserved like [`reserve_exact`](Self::reserve_exact)
    /// does, except that a page is taken from the page cache: a unique buffer
    /// is reclaimed when its allocation is large enough or reallocated, often
    /// in place, otherwise the data is copied into a new buffer.
    ///
    /// # Panics
    ///
    /// Panics if the new capacity exceeds `u32::MAX` minus the buffer
    /// header size, just under 4 GiB.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::{BytePageSize, BytesMut};
    ///
    /// let mut buf = BytesMut::with_page_size(BytePageSize::Size4);
    /// buf.extend_from_slice(b"hello");
    ///
    /// // at least half of the page is remaining, the buffer is kept
    /// buf.reserve_more();
    /// assert_eq!(buf.page_size(), BytePageSize::Size4);
    ///
    /// buf.extend_from_slice(&[0; 3000]);
    /// buf.reserve_more();
    /// assert_eq!(buf.page_size(), BytePageSize::Size8);
    /// assert_eq!(buf.capacity(), BytePageSize::Size8.capacity());
    /// assert_eq!(&buf[..5], b"hello");
    ///
    /// let mut buf = BytesMut::with_capacity(1000);
    /// buf.reserve_more();
    /// assert_eq!(buf.capacity(), 2000);
    ///
    /// let mut buf = BytesMut::with_capacity(1024 * 1024);
    /// buf.extend_from_slice(&vec![0; 1024 * 1024]);
    /// buf.reserve_more();
    /// assert_eq!(buf.capacity(), 1024 * 1024 + 64 * 1024);
    /// ```
    #[inline]
    pub fn reserve_more(&mut self) {
        self.storage.reserve_more();
    }

    /// Appends a byte slice to the buffer.
    ///
    /// Additional capacity is reserved automatically when needed.
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
    /// let buf = BytesMut::copy_from_slice(&b"abc"[..]);
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
}

impl_buf!(BytesMut {});

impl_slice_traits!(BytesMut);

impl_partial_eq!(BytesMut);

impl BufMut for BytesMut {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.storage.remaining()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        // This call will panic if `cnt` is too big
        self.storage.set_len(self.len() + cnt);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        self.storage.spare_mut()
    }

    #[inline]
    fn put<T: Buf>(&mut self, mut src: T)
    where
        Self: Sized,
    {
        self.reserve(src.remaining());
        while src.has_remaining() {
            let chunk = src.chunk();
            let len = chunk.len();
            self.put_slice(chunk);
            src.advance(len);
        }
    }

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        self.reserve(src.len());
        self.storage.put_slice_partial(src);
    }

    #[inline]
    fn put_u8(&mut self, n: u8) {
        self.reserve(1);
        self.storage.put_u8(n);
    }

    #[inline]
    fn put_i8(&mut self, n: i8) {
        self.put_u8(n as u8);
    }
}

/// Interop with the `bytes` crate: like `bytes::BytesMut`, the buffer grows on
/// demand, so `remaining_mut()` reports `usize::MAX - len` and `chunk_mut()`
/// is never empty. The native [`BufMut`] impl reports spare capacity instead.
unsafe impl bytes::buf::BufMut for BytesMut {
    #[inline]
    fn remaining_mut(&self) -> usize {
        usize::MAX - self.len()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        let remaining = BufMut::remaining_mut(self);
        assert!(
            cnt <= remaining,
            "cannot advance past `remaining_mut`: {cnt:?} <= {remaining:?}"
        );
        BufMut::advance_mut(self, cnt);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        if BufMut::remaining_mut(self) == 0 {
            self.reserve(64);
        }
        unsafe {
            let ptr = self.storage.as_ptr();
            bytes::buf::UninitSlice::from_raw_parts_mut(
                ptr.add(self.len()),
                BufMut::remaining_mut(self),
            )
        }
    }

    #[inline]
    fn put<T: bytes::buf::Buf>(&mut self, mut src: T)
    where
        Self: Sized,
    {
        self.reserve(src.remaining());
        while src.has_remaining() {
            let chunk = src.chunk();
            let len = chunk.len();
            BufMut::put_slice(self, chunk);
            src.advance(len);
        }
    }

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        BufMut::put_slice(self, src);
    }

    #[inline]
    fn put_bytes(&mut self, val: u8, cnt: usize) {
        self.reserve(cnt);
        unsafe {
            ptr::write_bytes(self.storage.as_ptr().add(self.len()), val, cnt);
            BufMut::advance_mut(self, cnt);
        }
    }

    #[inline]
    fn put_u8(&mut self, n: u8) {
        BufMut::put_u8(self, n);
    }

    #[inline]
    fn put_i8(&mut self, n: i8) {
        BufMut::put_i8(self, n);
    }
}

impl AsMut<[u8]> for BytesMut {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.storage.as_mut()
    }
}

impl DerefMut for BytesMut {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.storage.as_mut()
    }
}

impl Eq for BytesMut {}

impl PartialEq for BytesMut {
    #[inline]
    fn eq(&self, other: &BytesMut) -> bool {
        self.storage.as_ref() == other.storage.as_ref()
    }
}

impl borrow::BorrowMut<[u8]> for BytesMut {
    #[inline]
    fn borrow_mut(&mut self) -> &mut [u8] {
        self.as_mut()
    }
}

impl PartialEq<Bytes> for BytesMut {
    fn eq(&self, other: &Bytes) -> bool {
        other[..] == self[..]
    }
}

impl PartialEq<BytesMut> for Bytes {
    fn eq(&self, other: &BytesMut) -> bool {
        *other == *self
    }
}

impl_read!(BytesMut);

impl io::Write for BytesMut {
    fn write(&mut self, src: &[u8]) -> Result<usize, io::Error> {
        self.extend_from_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl fmt::Write for BytesMut {
    #[inline]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

impl Clone for BytesMut {
    #[inline]
    fn clone(&self) -> BytesMut {
        BytesMut::from(&self[..])
    }
}

impl FromIterator<u8> for BytesMut {
    fn from_iter<T: IntoIterator<Item = u8>>(into_iter: T) -> Self {
        let iter = into_iter.into_iter();
        let (min, maybe_max) = iter.size_hint();

        let mut out = BytesMut::with_capacity(maybe_max.unwrap_or(min));
        out.extend(iter);
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
        self.reserve(iter.size_hint().0);
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
        self.extend(iter.into_iter().copied());
    }
}

impl From<BytesMut> for Bytes {
    #[inline]
    fn from(b: BytesMut) -> Self {
        b.freeze()
    }
}

impl<'a> From<&'a [u8]> for BytesMut {
    #[inline]
    fn from(src: &'a [u8]) -> BytesMut {
        BytesMut::copy_from_slice(src)
    }
}

impl<const N: usize> From<[u8; N]> for BytesMut {
    #[inline]
    fn from(src: [u8; N]) -> BytesMut {
        BytesMut::copy_from_slice(src)
    }
}

impl<'a, const N: usize> From<&'a [u8; N]> for BytesMut {
    #[inline]
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
        match src.storage.try_into_vec() {
            Ok(storage) => BytesMut { storage },
            Err(storage) => BytesMut::copy_from_slice(storage.as_ref()),
        }
    }
}

impl From<&Bytes> for BytesMut {
    #[inline]
    fn from(src: &Bytes) -> BytesMut {
        BytesMut::copy_from_slice(&src[..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn growth_is_amortized() {
        let mut buf = BytesMut::with_capacity(0);
        let mut cap = buf.capacity();
        let mut reallocs = 0;
        for _ in 0..10_000 {
            buf.put_slice(b"abcdefgh");
            if buf.capacity() != cap {
                reallocs += 1;
                cap = buf.capacity();
            }
        }
        assert_eq!(buf.len(), 80_000);
        assert!(reallocs <= 16, "reallocs: {reallocs}");

        // writes through `io::Write` and `fmt::Write` grow the same way
        let mut buf = BytesMut::with_capacity(0);
        for i in 0..1000 {
            std::fmt::Write::write_fmt(&mut buf, format_args!("{i:08}")).unwrap();
        }
        assert_eq!(buf.len(), 8000);
        assert!(buf.capacity() < 16_000);
    }

    #[test]
    fn growth_of_little_data_is_exact() {
        let mut buf = BytesMut::copy_from_slice(b"hello");
        buf.reserve(64 * 1024);
        assert_eq!(buf.capacity(), 5 + 64 * 1024);

        // a buffer shared with split off `Bytes`
        let mut buf = BytesMut::with_capacity(1024);
        buf.extend_from_slice(&[1; 1024]);
        let head = buf.split_to(1000);
        buf.reserve(4096);
        assert_eq!(buf.capacity(), 24 + 4096);
        assert_eq!(&head[..], &[1; 1000][..]);
    }

    #[test]
    fn from_unique_bytes_reuses_buffer() {
        let mut buf = BytesMut::with_capacity(256);
        buf.extend_from_slice(&[1; 64]);
        let b = buf.freeze();
        let ptr = b.as_ptr();

        let mut m = BytesMut::from(b);
        assert_eq!(m.as_ptr(), ptr);
        assert_eq!(&m[..], &[1; 64][..]);
        assert_eq!(m.capacity(), 256);

        // spare capacity past the view is writable
        m.extend_from_slice(&[2; 192]);
        assert_eq!(m.as_ptr(), ptr);
        assert_eq!(&m[64..], &[2; 192][..]);
    }

    #[test]
    fn from_unique_bytes_subview() {
        let mut buf = BytesMut::with_capacity(256);
        buf.extend_from_slice(&[1; 128]);
        let mut b = buf.freeze();
        let head = b.split_to(32);
        drop(head);
        b.truncate(64);
        let ptr = b.as_ptr();

        let mut m = BytesMut::from(b);
        assert_eq!(m.as_ptr(), ptr);
        assert_eq!(m.len(), 64);
        assert_eq!(m.capacity(), 256 - 32);

        // the dropped tail of the view is spare capacity again
        m.extend_from_slice(&[3; 160]);
        assert_eq!(m.as_ptr(), ptr);
        assert_eq!(&m[..64], &[1; 64][..]);
        assert_eq!(&m[64..], &[3; 160][..]);
    }

    #[test]
    fn from_shared_bytes_copies() {
        let b = BytesMut::copy_from_slice([1; 64]).freeze();
        let b2 = b.clone();

        let mut m = BytesMut::from(b);
        assert_ne!(m.as_ptr(), b2.as_ptr());
        m[0] = 2;
        assert_eq!(&b2[..], &[1; 64][..]);

        // the buffer is still referenced by a `BytesMut`
        let mut buf = BytesMut::with_capacity(256);
        buf.extend_from_slice(&[1; 64]);
        let b = buf.take();
        let m = BytesMut::from(b);
        assert_ne!(m.as_ptr(), buf.as_ptr());
        buf.extend_from_slice(&[2; 64]);
        assert_eq!(&m[..], &[1; 64][..]);
    }

    // Run under miri: without `Acquire`, the header update races with the
    // read made by the other thread before it released its handle.
    #[test]
    fn from_bytes_synchronizes_with_release() {
        let b = BytesMut::copy_from_slice([1; 64]).freeze();
        let other = b.clone();
        let handle = std::thread::spawn(move || {
            let val = other[0];
            drop(other);
            val
        });

        let ptr = b.as_ptr();
        let mut storage = b.storage;
        let mut m = loop {
            match storage.try_into_vec() {
                Ok(storage) => break BytesMut { storage },
                Err(st) => {
                    storage = st;
                    std::thread::yield_now();
                }
            }
        };
        assert_eq!(m.as_ptr(), ptr);
        m[0] = 2;
        assert_eq!(handle.join().unwrap(), 1);
    }

    #[test]
    fn from_inline_and_static_bytes() {
        let m = BytesMut::from(Bytes::copy_from_slice(b"inline"));
        assert_eq!(&m[..], b"inline");

        let m = BytesMut::from(Bytes::from_static(&[1; 64]));
        assert_eq!(&m[..], &[1; 64][..]);
    }

    #[test]
    fn bvec_read() {
        use std::io::Read;

        let mut b = BytesMut::copy_from_slice(b"123");

        let mut buf = [0; 10];
        assert_eq!(b.read(&mut buf).unwrap(), 3);
        assert_eq!(b.len(), 0);
        assert_eq!(buf, [49, 50, 51, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn from_bytes_ref() {
        let b = Bytes::from_static(b"hello");
        let mut m = BytesMut::from(&b);
        m.extend_from_slice(b"!");
        assert_eq!(m, "hello!");
        assert_eq!(b, "hello");
    }
}
