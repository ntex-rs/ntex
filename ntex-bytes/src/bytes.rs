use std::{borrow, cmp, fmt, hash, mem, ops};

use crate::{Buf, BytesMut, buf::IntoIter, debug, storage::INLINE_CAP, storage::Storage};

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
    pub(crate) storage: Storage,
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
            storage: Storage::empty(),
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
            storage: Storage::from_static(bytes),
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
        self.storage.len()
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
        self.storage.is_empty()
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
        self.storage.is_inline()
    }

    /// Creates `Bytes` instance from slice, by copying it.
    pub fn copy_from_slice(data: &[u8]) -> Self {
        Bytes {
            storage: Storage::from_slice(data),
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
    pub fn slice(&self, range: impl ops::RangeBounds<usize>) -> Bytes {
        self.slice_checked(range)
            .expect("Requires that `begin <= end` and `end <= self.len()`")
    }

    /// Returns a slice of self for the provided range.
    ///
    /// Does nothing if `begin <= end` or `end <= self.len()`
    pub fn slice_checked(&self, range: impl ops::RangeBounds<usize>) -> Option<Bytes> {
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
                    storage: Storage::from_slice(&self[begin..end]),
                })
            } else {
                let mut ret = self.clone();
                unsafe {
                    ret.storage.set_end(end);
                    ret.storage.set_start(begin);
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
                    storage: self.storage.split_off(at, true),
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
                    storage: self.storage.split_to(at),
                })
            }
        } else {
            None
        }
    }

    /// Advance the internal cursor.
    ///
    /// Afterwards `self` contains elements `[cnt, len)`.
    /// This is an `O(1)` operation.
    ///
    /// # Examples
    ///
    /// ```
    /// use ntex_bytes::Bytes;
    ///
    /// let mut a = Bytes::copy_from_slice(&b"hello world"[..]);
    /// a.advance_to(5);
    ///
    /// assert_eq!(&a[..], b" world");
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if `cnt > len`.
    pub fn advance_to(&mut self, cnt: usize) {
        unsafe {
            self.storage.set_start(cnt);
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
        self.storage.truncate(len, true);
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
        self.storage.trimdown();
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
        self.storage = Storage::empty();
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

    #[inline]
    #[doc(hidden)]
    pub fn info(&self) -> crate::info::Info {
        self.storage.info()
    }
}

impl Buf for Bytes {
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
        self.advance_to(cnt)
    }
}

impl bytes::buf::Buf for Bytes {
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
        self.advance_to(cnt)
    }
}

impl Clone for Bytes {
    fn clone(&self) -> Bytes {
        Bytes {
            storage: self.storage.clone(),
        }
    }
}

impl AsRef<[u8]> for Bytes {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.storage.as_ref()
    }
}

impl ops::Deref for Bytes {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.storage.as_ref()
    }
}

impl From<&Bytes> for Bytes {
    fn from(src: &Bytes) -> Bytes {
        src.clone()
    }
}

impl From<Vec<u8>> for Bytes {
    /// Convert a `Vec` into a `Bytes`
    fn from(src: Vec<u8>) -> Bytes {
        Bytes {
            storage: Storage::from_slice(&src),
        }
    }
}

impl From<String> for Bytes {
    fn from(src: String) -> Bytes {
        Bytes {
            storage: Storage::from_slice(src.as_bytes()),
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
        self.storage.as_ref() == other.storage.as_ref()
    }
}

impl PartialOrd for Bytes {
    fn partial_cmp(&self, other: &Bytes) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bytes {
    fn cmp(&self, other: &Bytes) -> cmp::Ordering {
        self.storage.as_ref().cmp(other.storage.as_ref())
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
        fmt::Debug::fmt(&debug::BsDebug(self.storage.as_ref()), fmt)
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

impl borrow::Borrow<[u8]> for Bytes {
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
 * ===== PartialEq / PartialOrd =====
 *
 */

impl PartialEq<[u8]> for Bytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.storage.as_ref() == other
    }
}

impl<const N: usize> PartialEq<[u8; N]> for Bytes {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.storage.as_ref() == other.as_ref()
    }
}

impl PartialOrd<[u8]> for Bytes {
    fn partial_cmp(&self, other: &[u8]) -> Option<cmp::Ordering> {
        self.storage.as_ref().partial_cmp(other)
    }
}

impl<const N: usize> PartialOrd<[u8; N]> for Bytes {
    fn partial_cmp(&self, other: &[u8; N]) -> Option<cmp::Ordering> {
        self.storage.as_ref().partial_cmp(other.as_ref())
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
        self.storage.as_ref() == other.as_bytes()
    }
}

impl PartialOrd<str> for Bytes {
    fn partial_cmp(&self, other: &str) -> Option<cmp::Ordering> {
        self.storage.as_ref().partial_cmp(other.as_bytes())
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
        self.storage.as_ref().partial_cmp(&other[..])
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
        self.storage.as_ref().partial_cmp(other.as_bytes())
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::BufMut;

    const LONG: &[u8] = b"mary had a1 little la2mb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \
        mary had a little lamb, little lamb, little lamb, little lamb, little lamb, little lamb \0";

    #[test]
    #[allow(
        clippy::op_ref,
        clippy::len_zero,
        clippy::nonminimal_bool,
        clippy::unnecessary_fallible_conversions
    )]
    fn bytes() {
        let mut b = Bytes::from(LONG.to_vec());
        b.advance_to(10);
        assert_eq!(&b, &LONG[10..]);
        b.advance_to(10);
        assert_eq!(&b[..], &LONG[20..]);
        assert_eq!(&b, &LONG[20..]);
        b.clear();
        assert!(b.is_inline());
        assert!(b.is_empty());
        assert!(b.len() == 0);

        let b = Bytes::from(b"123");
        assert!(&b"12"[..] > &b);
        assert!("123" == &b);
        assert!("12" > &b);

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
        <Bytes as Buf>::advance(&mut b, 10);
        assert_eq!(Buf::chunk(&b), &LONG[20..]);

        let mut h: HashMap<Bytes, usize> = HashMap::default();
        h.insert(b.clone(), 1);
        assert_eq!(h.get(&b), Some(&1));

        let mut b = BytesMut::try_from(LONG).unwrap();
        assert_eq!(b, LONG);
        assert_eq!(<BytesMut as Buf>::remaining(&b), LONG.len());
        assert_eq!(<BytesMut as BufMut>::remaining_mut(&b), 0);
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

        let mut iter = Bytes::from(LONG.to_vec()).into_iter();
        assert_eq!(iter.next(), Some(LONG[0]));
        assert_eq!(iter.next(), Some(LONG[1]));
        assert_eq!(iter.next(), Some(LONG[2]));
        assert_eq!(iter.next(), Some(LONG[3]));
    }
}
