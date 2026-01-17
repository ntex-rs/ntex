use std::borrow::{Borrow, BorrowMut};
use std::ops::{Deref, DerefMut};
use std::{fmt, ptr};

use crate::storage::StorageVec;
use crate::{Buf, BufMut, Bytes, buf::IntoIter, buf::UninitSlice, debug};

/// A unique reference to a contiguous slice of memory.
///
/// `BytesMut` represents a unique view into a potentially shared memory region.
/// Given the uniqueness guarantee, owners of `BytesMut` handles are able to
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
    /// The returned `BytesMut` will be able to hold at least `capacity` bytes
    /// without reallocating.
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
            storage: StorageVec::with_capacity(capacity),
        }
    }

    /// Creates a new `BytesMut` from slice, by copying it.
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
    pub fn new() -> BytesMut {
        BytesMut {
            storage: StorageVec::with_capacity(104),
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
            storage: self.storage.freeze(),
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
            self.storage.set_start(cnt as u32);
        }
    }

    /// Splits the bytes into two at the given index.
    ///
    /// Does nothing if `at > len`.
    #[inline]
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
    /// The [`split_off`] method can emulate `truncate`, but this causes the
    /// excess bytes to be returned instead of dropped.
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
    ///
    /// [`split_off`]: #method.split_off
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        self.storage.truncate(len);
    }

    /// Clears the buffer, removing all data.
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
    /// # Panics
    ///
    /// This method will panic if `len` is out of bounds for the underlying
    /// slice or if it comes after the `end` of the configured window.
    #[inline]
    #[allow(clippy::missing_safety_doc)]
    pub unsafe fn set_len(&mut self, len: usize) {
        self.storage.set_len(len)
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
    ///
    /// # Panics
    ///
    /// Panics if the new capacity overflows `usize`.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.storage.reserve(additional);
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

impl Buf for BytesMut {
    #[inline]
    fn remaining(&self) -> usize {
        self.len()
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        self.storage.as_ref()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        self.advance_to(cnt);
    }
}

impl BufMut for BytesMut {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.capacity() - self.len()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        // This call will panic if `cnt` is too big
        self.storage.set_len(self.len() + cnt);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        let len = self.len();

        unsafe {
            // This will never panic as `len` can never become invalid
            let ptr = &mut self.storage.as_raw()[len..];
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
        self.storage.put_u8(n);
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
        self.storage.as_ref()
    }

    #[inline]
    fn advance(&mut self, cnt: usize) {
        self.advance_to(cnt);
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
            let ptr = self.storage.as_ptr();
            bytes::buf::UninitSlice::from_raw_parts_mut(ptr.add(len), self.capacity() - len)
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
        self.storage.as_ref()
    }
}

impl AsMut<[u8]> for BytesMut {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.storage.as_mut()
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

impl PartialEq<Bytes> for BytesMut {
    fn eq(&self, other: &Bytes) -> bool {
        other[..] == self[..]
    }
}

impl fmt::Debug for BytesMut {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&debug::BsDebug(self.storage.as_ref()), fmt)
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

        for (idx, b) in iter.enumerate() {
            if idx >= lower {
                self.reserve(1);
            }
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

impl PartialEq<BytesMut> for Bytes {
    fn eq(&self, other: &BytesMut) -> bool {
        other[..] == self[..]
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
        //src.try_mut().unwrap_or_else(|src| BytesMut::copy_from_slice(&src[..]))
        BytesMut::copy_from_slice(&src[..])
    }
}

impl From<&Bytes> for BytesMut {
    #[inline]
    fn from(src: &Bytes) -> BytesMut {
        BytesMut::copy_from_slice(&src[..])
    }
}
