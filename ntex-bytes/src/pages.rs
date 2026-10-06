#![allow(clippy::missing_panics_doc, clippy::box_collection)]
use std::{borrow::Borrow, cell::Cell, cmp, collections::VecDeque, fmt, io, mem, ops};

use crate::{Buf, BufMut, BytePageSize, ByteString, Bytes, BytesMut};
use crate::{buf::UninitSlice, stvec::StorageVec};

/// A growable sequence of byte pages.
///
/// Data is stored in fixed-capacity pages selected by [`BytePageSize`]. This
/// avoids reallocating and copying one large contiguous buffer as data grows.
pub struct BytePages {
    st: Option<Box<Inner>>,
    current: Option<StorageVec>,
}

#[derive(Debug)]
struct Inner {
    size: BytePageSize,
    /// Total length of `pages`, so `len()` does not walk them.
    len: usize,
    pages: VecDeque<BytePage>,
}

thread_local! {
    static CACHE: Cell<Option<Box<Vec<Box<Inner>>>>> = Cell::new(Some(Box::default()));
}
const CACHE_SIZE: usize = 128;

/// Appended data up to this size is copied into the current page.
const APPEND_COPY_LIMIT: usize = 4096;

impl BytePages {
    /// Creates a new `BytePages` with the specified page size.
    ///
    /// Pages are allocated lazily using the specified capacity category.
    ///
    /// # Panics
    ///
    /// Panics if `size` is [`BytePageSize::Unset`].
    pub fn new(size: BytePageSize) -> Self {
        assert!(size != BytePageSize::Unset, "Page size cannot be Unset");

        // the cache is unavailable while the thread-local is being destroyed
        let cached = CACHE
            .try_with(|c| {
                let mut cache = c.take()?;
                let item = cache.pop();
                c.set(Some(cache));
                item
            })
            .ok()
            .flatten();

        let st = if let Some(mut item) = cached {
            item.size = size;
            item
        } else {
            Box::new(Inner {
                size,
                len: 0,
                pages: VecDeque::with_capacity(8),
            })
        };

        BytePages {
            st: Some(st),
            current: None,
        }
    }

    fn pages(&self) -> &VecDeque<BytePage> {
        &self.st.as_ref().unwrap().pages
    }

    // Pages are only added and removed through these methods, which keep
    // `Inner::len` up to date. A page in the list is never modified in place.
    fn push_front(&mut self, page: BytePage) {
        let st = self.st.as_mut().unwrap();
        st.len += page.len();
        st.pages.push_front(page);
    }

    fn pop_front(&mut self) -> Option<BytePage> {
        let st = self.st.as_mut().unwrap();
        let page = st.pages.pop_front()?;
        st.len -= page.len();
        Some(page)
    }

    fn push_back(&mut self, page: BytePage) {
        let st = self.st.as_mut().unwrap();
        st.len += page.len();
        let pages = &mut st.pages;
        pages.push_back(page);

        #[cfg(feature = "overuse")]
        if pages.len() == 128 {
            log::debug!(
                "Number of pages {}\n{:?}",
                pages.len(),
                backtrace::Backtrace::new()
            );
        }
    }

    /// Returns the capacity category used for new pages.
    pub fn page_size(&self) -> BytePageSize {
        self.st.as_ref().unwrap().size
    }

    /// Sets the page size for new pages.
    ///
    /// # Panics
    ///
    /// Panics if `size` is [`BytePageSize::Unset`].
    pub fn set_page_size(&mut self, size: BytePageSize) {
        assert!(size != BytePageSize::Unset, "Page size cannot be Unset");
        self.st.as_mut().unwrap().size = size;
    }

    /// Inserts a non-empty page at the front of the collection.
    ///
    /// Returns whether a page was inserted.
    pub fn prepend<T>(&mut self, buf: T) -> bool
    where
        BytePage: From<T>,
    {
        let p = BytePage::from(buf);
        if p.is_empty() {
            false
        } else {
            self.push_front(p);
            true
        }
    }

    /// Appends a page to the back of the collection.
    ///
    /// Empty pages are ignored. If the current page holds no data and `buf` is
    /// a unique buffer with spare capacity, it becomes the new current page.
    /// Data of up to 4 KiB is copied into the current page, and into new
    /// pages as needed. Larger data is added as a separate page without
    /// copying: the filled part of the current page is split off in front of
    /// it, and the spare capacity of the current page stays available for
    /// later writes.
    pub fn append<T>(&mut self, buf: T)
    where
        BytePage: From<T>,
    {
        let p = BytePage::from(buf);
        if !p.is_empty() {
            if self.current_len() == 0 {
                match p.into_storage() {
                    Ok(st) => {
                        self.current = Some(st);
                    }
                    Err(page) => {
                        // add buffer to the page list
                        self.push_back(page);
                    }
                }
            } else if p.len() <= APPEND_COPY_LIMIT {
                self.put_slice(p.as_ref());
            } else {
                // the current page is never full, its spare capacity is kept
                if let Some(st) = self.current.as_mut() {
                    let head = st.split_to(st.len());
                    self.push_back(<BytePage as From<Bytes>>::from(Bytes { storage: head }));
                }
                self.push_back(p);
            }
        }
    }

    #[inline]
    /// Appends the given bytes to this page object.
    ///
    /// Tries to write the data into the current page first. If there
    /// is insufficient space, one or more new pages are allocated as
    /// needed, and the remaining data is copied into them.
    pub fn extend_from_slice(&mut self, extend: &[u8]) {
        self.put_slice(extend);
    }

    #[inline]
    /// Returns the total number of buffered bytes.
    pub fn len(&self) -> usize {
        self.st.as_ref().unwrap().len + self.current_len()
    }

    fn current_len(&self) -> usize {
        self.current
            .as_ref()
            .map(StorageVec::len)
            .unwrap_or_default()
    }

    // spare capacity of the current page
    fn spare(&self) -> usize {
        self.current
            .as_ref()
            .map(StorageVec::remaining)
            .unwrap_or_default()
    }

    #[inline]
    /// Returns `true` if no bytes are buffered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    /// Returns the number of allocated pages containing buffered data.
    pub fn num_pages(&self) -> usize {
        if self.current_len() == 0 {
            self.pages().len()
        } else {
            self.pages().len() + 1
        }
    }

    /// Removes and returns the first page from the collection.
    ///
    /// The current writable page is returned last. Returns `None` if there are
    /// no pages with data, an empty current page keeps its spare capacity for
    /// later writes.
    pub fn take(&mut self) -> Option<BytePage> {
        if let Some(page) = self.pop_front() {
            Some(page)
        } else if self.current_len() == 0 {
            None
        } else {
            self.current.take().map(BytePage::from)
        }
    }

    #[inline]
    /// Appends all buffered data to another [`BytePages`] value, `self` is
    /// left unchanged.
    ///
    /// Pages are shared with `pages` rather than copied, unless they are small
    /// enough to be copied into the current page of `pages`, see
    /// [`append`](Self::append).
    pub fn copy_to(&self, pages: &mut BytePages) {
        for p in self.pages() {
            pages.append(p.clone());
        }

        if let Some(st) = &self.current {
            // an immutable view, `st` stays the only handle that can write
            // to the spare capacity
            pages.append(Bytes {
                storage: st.shallow_freeze(),
            });
        }
    }

    #[inline]
    /// Moves all buffered data to the back of another [`BytePages`] value,
    /// leaving `self` empty.
    ///
    /// Pages are moved according to the rules of [`append`](Self::append).
    pub fn move_to(&mut self, pages: &mut BytePages) {
        while let Some(page) = self.take() {
            pages.append(page);
        }
    }

    /// Splits the buffer into two at the given index.
    ///
    /// Afterwards, `self` contains elements `[at, len)`, and the returned [`BytePages`]
    /// contains elements `[0, at)`. If `at > len`, all data is moved.
    ///
    /// Depending on the underlying storage, this operation might be `O(1)` or could
    /// involve a memory copy.
    #[must_use]
    pub fn split_to(&mut self, at: usize) -> BytePages {
        let mut pages = BytePages::new(self.page_size());
        self.split_into(at, &mut pages);
        pages
    }

    /// Splits the buffer, adding the resulting items to the supplied pages object.
    ///
    /// Afterwards, `self` contains elements `[at, len)`, and elements `[0, at)`
    /// are appended to `to`. If `at > len`, all data is moved.
    ///
    /// Depending on the underlying storage, this operation might be `O(1)` or could
    /// involve a memory copy.
    pub fn split_into(&mut self, mut at: usize, to: &mut BytePages) {
        while let Some(mut page) = self.pop_front() {
            let len = cmp::min(page.len(), at);
            to.append(page.split_to(len));

            if !page.is_empty() {
                self.push_front(page);
                return;
            }
            at -= len;
        }
        if at > 0
            && let Some(mut st) = self.current.take()
        {
            if at < st.len() {
                // the remainder stays writable, so its spare capacity is kept
                to.append(Bytes {
                    storage: st.split_to(at),
                });
                self.current = Some(st);
            } else if st.len() == 0 {
                self.current = Some(st);
            } else {
                to.append(BytePage::from(st));
            }
        }
    }

    /// Clears the buffer, removing all data.
    #[inline]
    pub fn clear(&mut self) {
        while self.take().is_some() {}
    }

    /// Drains all pages into one immutable [`Bytes`] value.
    #[inline]
    #[must_use]
    pub fn freeze(&mut self) -> Bytes {
        let pages = self.num_pages();
        if pages == 0 || self.is_empty() {
            Bytes::new()
        } else if pages == 1 {
            self.take().unwrap().freeze()
        } else {
            let mut buf = BytesMut::with_capacity(self.len());
            while let Some(p) = self.take() {
                buf.extend_from_slice(&p);
            }
            buf.freeze()
        }
    }

    #[inline]
    /// Moves the current writable page from `pages` if this value is empty.
    pub fn try_get_current_from(&mut self, pages: &mut BytePages) {
        if self.pages().is_empty()
            && self.current.is_none()
            && let Some(st) = pages.current.take()
        {
            self.current = Some(st);
        }
    }

    /// Provides mutable access to the current writable page.
    ///
    /// The current page, or a new page of [`page_size`](Self::page_size) if
    /// there is none, is passed to `f` as a [`BytesMut`]. After `f` returns,
    /// the buffer becomes the current page again. If its length has reached
    /// the page size, it is pushed onto the page list instead. If `f` changed
    /// the buffer's capacity (for example by reserving more space), the page
    /// is no longer returned to the page cache when it is released.
    ///
    /// This is a low-level API intended for ntex internals and may change
    /// without notice.
    ///
    /// # Panics
    ///
    /// `f` must not panic. The page is not reference-counted while `f` runs,
    /// so unwinding out of `f` releases it twice, which is undefined behavior.
    #[doc(hidden)]
    #[deprecated(
        since = "1.10.0",
        note = "not panic safe, use the `BufMut` methods of `BytePages` instead"
    )]
    pub fn with_bytes_mut<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut BytesMut) -> R,
    {
        let mut buf = BytesMut::new();
        let res = f(&mut buf);
        self.append(buf);
        res
    }

    fn with_current<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut StorageVec) -> R,
    {
        let mut st = self
            .current
            .take()
            .unwrap_or_else(|| StorageVec::sized(self.page_size()));
        let result = f(&mut st);

        // a full page moves to the page list
        if st.is_full() {
            self.push_back(BytePage::from(st));
        } else {
            self.current = Some(st);
        }

        result
    }
}

impl Drop for BytePages {
    fn drop(&mut self) {
        if let Some(mut st) = self.st.take() {
            st.pages.clear();
            // a large write must not pin its page list in the cache
            st.pages.shrink_to(8);
            st.len = 0;
            // the cache is unavailable while the thread-local is being destroyed
            let _ = CACHE.try_with(move |c| {
                if let Some(mut cache) = c.take() {
                    if cache.len() < CACHE_SIZE {
                        cache.push(st);
                    }
                    c.set(Some(cache));
                }
            });
        }
    }
}

impl fmt::Debug for BytePages {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = fmt.debug_tuple("BytePages");
        for p in self.pages() {
            f.field(p);
        }
        if let Some(st) = &self.current {
            f.field(&crate::debug::BsDebug(st.as_ref()));
        }
        f.finish()
    }
}

impl Default for BytePages {
    fn default() -> Self {
        BytePages::new(BytePageSize::Size16)
    }
}

/// Pages are allocated on demand, so `remaining_mut()` reports
/// `usize::MAX - len` and `chunk_mut()` is never empty. The chunk covers the
/// spare capacity of the current page only.
impl BufMut for BytePages {
    #[inline]
    fn remaining_mut(&self) -> usize {
        usize::MAX - self.len()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        if cnt == 0 {
            return;
        }
        let spare = self.spare();
        assert!(
            cnt <= spare,
            "cannot advance past the current page: {cnt:?} <= {spare:?}"
        );
        let st = self.current.as_mut().unwrap();
        st.set_len(st.len() + cnt);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        if self.spare() == 0 {
            if let Some(st) = self.current.take() {
                self.push_back(BytePage::from(st));
            }
            self.current = Some(StorageVec::sized(self.page_size()));
        }
        // `current` is set, a new page is allocated above if there is no spare capacity
        self.current.as_mut().unwrap().spare_mut()
    }

    fn put<T: Buf>(&mut self, mut src: T)
    where
        Self: Sized,
    {
        while src.has_remaining() {
            let chunk = src.chunk();
            let len = chunk.len();
            self.put_slice(chunk);
            src.advance(len);
        }
    }

    fn put_slice(&mut self, mut src: &[u8]) {
        while !src.is_empty() {
            let amount = self.with_current(|st| st.put_slice_partial(src));

            src = &src[amount..];
        }
    }

    #[inline]
    fn put_u8(&mut self, n: u8) {
        self.with_current(|st| st.put_u8(n));
    }

    #[inline]
    fn put_i8(&mut self, n: i8) {
        self.put_u8(n as u8);
    }
}

impl Clone for BytePages {
    fn clone(&self) -> Self {
        let size = self.page_size();
        let mut pages = BytePages::new(size);
        self.copy_to(&mut pages);
        pages
    }
}

impl io::Write for BytePages {
    fn write(&mut self, src: &[u8]) -> Result<usize, io::Error> {
        self.put_slice(src);
        Ok(src.len())
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl From<BytePages> for Bytes {
    /// A single page is converted without copying.
    fn from(mut pages: BytePages) -> Bytes {
        pages.freeze()
    }
}

impl From<BytePages> for BytesMut {
    /// A single page is converted without copying if nothing else refers to
    /// its buffer.
    fn from(mut pages: BytePages) -> BytesMut {
        if pages.num_pages() == 1 {
            return BytesMut::from(pages.take().unwrap());
        }

        let mut buf = BytesMut::with_capacity(pages.len());
        while let Some(p) = pages.take() {
            buf.extend_from_slice(&p);
        }
        buf
    }
}

/// A contiguous chunk stored by [`BytePages`].
pub struct BytePage {
    inner: StorageType,
}

enum StorageType {
    Bytes(Bytes),
    Storage(StorageVec),
    Vec(Vec<u8>),
}

impl BytePage {
    #[inline]
    /// Returns the number of bytes contained in this `BytePage`.
    pub fn len(&self) -> usize {
        self.as_ref().len()
    }

    #[inline]
    /// Returns `true` if the page is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    /// Returns a raw pointer to the data.
    ///
    /// # Safety
    ///
    /// The returned pointer may only be dereferenced while the page is neither
    /// moved, modified nor dropped, and only for [`len`](Self::len) bytes. An
    /// inline page stores its data inside the `BytePage` itself, so moving the
    /// page invalidates the pointer, see [`is_inline`](Self::is_inline).
    pub unsafe fn as_ptr(&self) -> *const u8 {
        unsafe {
            match &self.inner {
                StorageType::Bytes(b) => b.storage.as_ptr(),
                StorageType::Storage(b) => b.as_ptr(),
                StorageType::Vec(b) => b.as_ptr(),
            }
        }
    }

    #[inline]
    #[doc(hidden)]
    /// Returns the kind of storage backing this page.
    pub fn info(&self) -> crate::info::PageKind {
        match &self.inner {
            StorageType::Bytes(_) => crate::info::PageKind::Bytes,
            StorageType::Storage(_) => crate::info::PageKind::Storage,
            StorageType::Vec(_) => crate::info::PageKind::Vec,
        }
    }

    #[inline]
    #[doc(hidden)]
    /// Returns `true` if the data is stored inside the `BytePage` itself.
    ///
    /// Moving an inline page moves its data, pointers from `as_ptr()` do not
    /// survive the move.
    pub fn is_inline(&self) -> bool {
        matches!(&self.inner, StorageType::Bytes(b) if b.is_inline())
    }

    /// Splits the buffer into two at the given index.
    ///
    /// Afterwards, `self` contains elements `[at, len)`, and the returned `BytePage`
    /// contains elements `[0, at)`. If `at > len`, all data is moved.
    ///
    /// Depending on the underlying storage, this operation might be `O(1)` or could
    /// involve a memory copy.
    #[must_use]
    pub fn split_to(&mut self, at: usize) -> BytePage {
        match &mut self.inner {
            StorageType::Bytes(b) => {
                let buf = b.split_to(cmp::min(at, b.len()));
                BytePage {
                    inner: StorageType::Bytes(buf),
                }
            }
            StorageType::Storage(_) => {
                let inner = mem::replace(&mut self.inner, StorageType::Bytes(Bytes::new()));
                if let StorageType::Storage(st) = inner {
                    self.inner = StorageType::Bytes(Bytes {
                        storage: st.freeze(),
                    });
                    self.split_to(at)
                } else {
                    unreachable!()
                }
            }
            StorageType::Vec(_) => {
                let inner = mem::replace(&mut self.inner, StorageType::Bytes(Bytes::new()));
                if let StorageType::Vec(b) = inner {
                    self.inner = StorageType::Bytes(Bytes::copy_from_slice(&b));
                    self.split_to(at)
                } else {
                    unreachable!()
                }
            }
        }
    }

    /// Advance the internal cursor.
    ///
    /// Afterwards `self` contains elements `[cnt, len)`.
    /// This is an `O(1)` operation, except for pages backed by a `Vec<u8>`,
    /// whose remaining data is copied.
    ///
    /// # Panics
    ///
    /// Panics if `cnt > len`.
    #[inline]
    pub fn advance_to(&mut self, cnt: usize) {
        match &mut self.inner {
            StorageType::Bytes(b) => b.advance_to(cnt),
            StorageType::Storage(b) => unsafe { b.set_start(cnt) },
            StorageType::Vec(b) => {
                assert!(
                    cnt <= b.len(),
                    "cannot advance past the end of the buffer, cnt:{cnt} len:{}",
                    b.len()
                );
                self.inner = StorageType::Bytes(Bytes::copy_from_slice(&b[cnt..]));
            }
        }
    }

    /// Converts `self` into an immutable `Bytes`.
    #[inline]
    #[must_use]
    pub fn freeze(self) -> Bytes {
        match self.inner {
            StorageType::Bytes(b) => b,
            StorageType::Storage(st) => Bytes {
                storage: st.freeze(),
            },
            StorageType::Vec(v) => Bytes::from(v),
        }
    }

    fn into_storage(self) -> Result<StorageVec, Self> {
        if let StorageType::Storage(st) = self.inner {
            // SAFETY: Converting back to `StorageVec` requires uniqueness.
            if !st.is_full() && st.is_unique() {
                Ok(st)
            } else {
                Err(Self {
                    inner: StorageType::Storage(st),
                })
            }
        } else {
            Err(self)
        }
    }
}

impl Clone for BytePage {
    fn clone(&self) -> Self {
        let inner = match &self.inner {
            StorageType::Bytes(b) => StorageType::Bytes(b.clone()),
            // The clone is an immutable view, `st` must stay the only
            // handle that can modify the shared header and spare capacity
            StorageType::Storage(st) => StorageType::Bytes(Bytes {
                storage: st.shallow_freeze(),
            }),
            StorageType::Vec(b) => StorageType::Bytes(Bytes::copy_from_slice(b)),
        };

        Self { inner }
    }
}

impl AsRef<[u8]> for BytePage {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match &self.inner {
            StorageType::Bytes(b) => b.as_ref(),
            StorageType::Storage(b) => b.as_ref(),
            StorageType::Vec(b) => b.as_ref(),
        }
    }
}

impl Borrow<[u8]> for BytePage {
    #[inline]
    fn borrow(&self) -> &[u8] {
        self.as_ref()
    }
}

impl From<Bytes> for BytePage {
    fn from(buf: Bytes) -> Self {
        BytePage {
            inner: StorageType::Bytes(buf),
        }
    }
}

impl<'a> From<&'a Bytes> for BytePage {
    fn from(buf: &'a Bytes) -> Self {
        BytePage {
            inner: StorageType::Bytes(buf.clone()),
        }
    }
}

impl From<BytesMut> for BytePage {
    fn from(buf: BytesMut) -> Self {
        BytePage {
            inner: StorageType::Storage(buf.storage),
        }
    }
}

impl From<ByteString> for BytePage {
    fn from(s: ByteString) -> Self {
        s.into_bytes().into()
    }
}

impl<'a> From<&'a ByteString> for BytePage {
    fn from(s: &'a ByteString) -> Self {
        s.clone().into_bytes().into()
    }
}

impl From<StorageVec> for BytePage {
    fn from(buf: StorageVec) -> Self {
        BytePage {
            inner: StorageType::Storage(buf),
        }
    }
}

impl From<Vec<u8>> for BytePage {
    fn from(buf: Vec<u8>) -> Self {
        BytePage {
            inner: StorageType::Vec(buf),
        }
    }
}

impl From<&'static str> for BytePage {
    fn from(buf: &'static str) -> Self {
        Bytes::from_static(buf.as_bytes()).into()
    }
}

impl From<&'static [u8]> for BytePage {
    fn from(buf: &'static [u8]) -> Self {
        Bytes::from_static(buf).into()
    }
}

impl<const N: usize> From<&'static [u8; N]> for BytePage {
    fn from(buf: &'static [u8; N]) -> Self {
        Bytes::from_static(buf).into()
    }
}

impl From<BytePage> for Bytes {
    fn from(page: BytePage) -> Self {
        match page.inner {
            StorageType::Bytes(b) => b,
            StorageType::Storage(storage) => BytesMut { storage }.freeze(),
            StorageType::Vec(v) => Bytes::copy_from_slice(&v),
        }
    }
}

impl From<BytePage> for BytesMut {
    fn from(page: BytePage) -> Self {
        match page.inner {
            StorageType::Bytes(b) => b.into(),
            // clones of the page may still read the data
            StorageType::Storage(storage) => {
                if storage.is_unique() {
                    BytesMut { storage }
                } else {
                    BytesMut::copy_from_slice(storage.as_ref())
                }
            }
            StorageType::Vec(v) => BytesMut::copy_from_slice(&v),
        }
    }
}

impl PartialEq for BytePage {
    fn eq(&self, other: &BytePage) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl_partial_eq!(BytePage);

impl_read!(BytePage);

impl ops::Deref for BytePage {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_ref()
    }
}

impl fmt::Debug for BytePage {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&crate::debug::BsDebug(self.as_ref()), fmt)
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use super::*;

    #[test]
    #[allow(clippy::op_ref, clippy::cmp_owned)]
    fn page_eq_and_read() {
        use std::io::Read;

        let mut page = BytePage::from(Vec::from(&b"hello"[..]));
        assert_eq!(page, b"hello"[..]);
        assert_eq!(page, *b"hello");
        assert_eq!(page, b"hello");
        assert_eq!(page, &b"hello"[..]);
        assert_eq!(page, "hello");
        assert_eq!(page, *"hello");
        assert_eq!(page, b"hello".to_vec());
        assert_eq!(page, String::from("hello"));
        assert_eq!(b"hello"[..], page);
        assert_eq!(*b"hello", page);
        assert_eq!(b"hello", page);
        assert_eq!(&b"hello"[..], page);
        assert_eq!("hello", page);
        assert_eq!(*"hello", page);
        assert_eq!(b"hello".to_vec(), page);
        assert_eq!(String::from("hello"), page);
        assert_ne!(page, "hell");

        let mut buf = [0u8; 3];
        assert_eq!(page.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"hel");
        assert_eq!(page, "lo");
        assert_eq!(page.read(&mut buf).unwrap(), 2);
        assert_eq!(page.read(&mut buf).unwrap(), 0);
        assert!(page.is_empty());
    }

    #[test]
    fn page_info() {
        use crate::info::PageKind;

        let p = BytePage::from(Bytes::copy_from_slice(&[1; 64]));
        assert_eq!(p.info(), PageKind::Bytes);
        let p = BytePage::from(BytesMut::copy_from_slice(&[1; 64][..]));
        assert_eq!(p.info(), PageKind::Storage);
        let p = BytePage::from(vec![1; 64]);
        assert_eq!(p.info(), PageKind::Vec);
        assert!(!p.is_inline());

        assert!(BytePage::from(Bytes::copy_from_slice(&[1; 4])).is_inline());
        assert!(!BytePage::from(Bytes::copy_from_slice(&[1; 64])).is_inline());
        assert!(!BytePage::from(BytesMut::copy_from_slice(&[1; 4][..])).is_inline());
    }

    #[test]
    fn append_copies_small_and_splits_for_large() {
        let cap = BytePageSize::Size16.capacity();
        let mut pages = BytePages::new(BytePageSize::Size16);
        pages.extend_from_slice(b"head\r\n");

        // small data is copied into the current page
        pages.append(Bytes::copy_from_slice(&[1; APPEND_COPY_LIMIT]));
        assert_eq!(pages.num_pages(), 1);
        assert_eq!(pages.current_len(), 6 + APPEND_COPY_LIMIT);

        // large data is not copied, the filled part is split off in front
        let body = Bytes::copy_from_slice(&[2; APPEND_COPY_LIMIT + 1]);
        let body_ptr = body.as_ptr();
        pages.append(body.clone());
        assert_eq!(pages.num_pages(), 2);
        assert_eq!(pages.current_len(), 0);
        assert_eq!(pages.spare(), cap - 6 - APPEND_COPY_LIMIT);

        // later writes use the spare capacity of the current page
        let page_ptr = pages.pages()[0].as_ref().as_ptr();
        pages.extend_from_slice(b"\r\n");
        assert_eq!(pages.num_pages(), 3);
        let tail_ptr = pages.current.as_ref().unwrap().as_ref().as_ptr();
        assert_eq!(tail_ptr, page_ptr.wrapping_add(6 + APPEND_COPY_LIMIT));

        let mut expected = b"head\r\n".to_vec();
        expected.extend_from_slice(&[1; APPEND_COPY_LIMIT]);
        expected.extend_from_slice(&body);
        expected.extend_from_slice(b"\r\n");
        assert_eq!(pages.len(), expected.len());

        let first = pages.take().unwrap();
        let second = pages.take().unwrap();
        assert_eq!(second.as_ref().as_ptr(), body_ptr);
        let third = pages.take().unwrap();
        assert!(pages.take().is_none());
        let data = [first.as_ref(), second.as_ref(), third.as_ref()].concat();
        assert_eq!(data, expected);

        // draining the pages keeps an empty current page for later writes
        pages.extend_from_slice(b"x");
        pages.append(body);
        assert_eq!(pages.take().unwrap().as_ref(), b"x");
        assert_eq!(pages.take().unwrap().len(), APPEND_COPY_LIMIT + 1);
        assert!(pages.take().is_none());
        assert_eq!(pages.num_pages(), 0);
        assert_eq!(pages.spare(), cap - 1);
    }

    #[test]
    fn pages() {
        let cap = BytePageSize::Size8.capacity();
        unsafe {
            // pages
            let mut pages = BytePages::new(BytePageSize::Size8);
            assert!(pages.is_empty());
            assert_eq!(pages.len(), 0);
            assert_eq!(pages.num_pages(), 0);
            pages.extend_from_slice(b"b");
            assert_eq!(pages.len(), 1);
            assert_eq!(pages.num_pages(), 1);
            pages.extend_from_slice("a".repeat(9 * 1024).as_bytes());
            assert_eq!(pages.len(), 9217);
            assert_eq!(pages.num_pages(), 2);
            assert!(!pages.is_empty());

            let mut pgs = BytePages::new(BytePageSize::Size8);
            pgs.put_i8(b'a' as i8);
            let p = pgs.take().unwrap();
            assert_eq!(p.len(), 1);
            assert_eq!(p.as_ref(), b"a");

            pgs.extend_from_slice("a".repeat(cap - 1).as_bytes());
            assert_eq!(pgs.num_pages(), 1);
            pgs.put_u8(b'a');
            assert_eq!(pgs.num_pages(), 1);
            assert!(pgs.current.is_none());

            pgs.put_u8(b'a');
            assert_eq!(pgs.num_pages(), 2);

            pgs.append(Bytes::copy_from_slice("a".repeat(cap).as_bytes()));
            assert_eq!(pgs.num_pages(), 3);
            assert_eq!(pgs.current_len(), 0);
            assert_eq!(pgs.spare(), cap - 1);

            // page
            let p = pages.take().unwrap();
            assert_eq!(p.len(), cap);
            let p = pages.take().unwrap();
            assert_eq!(p.len(), 9217 - cap);
            assert!(!p.is_empty());
            assert_eq!(p.as_ref().as_ptr(), p.as_ptr());
            assert_eq!(p.as_ref(), "a".repeat(9217 - cap).as_bytes());
            assert!(pages.take().is_none());

            let p = BytePage::from(Bytes::copy_from_slice(b"123"));
            assert_eq!(p.len(), 3);
            assert!(!p.is_empty());
            assert_eq!(p.as_ref(), b"123");
            assert_eq!(p.as_ref().as_ptr(), p.as_ptr());

            let p = BytePage::from(&b"123"[..]);
            assert_eq!(p.len(), 3);
            assert!(!p.is_empty());
            assert_eq!(p.as_ref(), b"123");
            assert_eq!(p.as_ref().as_ptr(), p.as_ptr());

            let p = BytePage::from(b"123");
            assert_eq!(p.len(), 3);
            assert!(!p.is_empty());
            assert_eq!(p.as_ref(), b"123");
            assert_eq!(p.as_ref().as_ptr(), p.as_ptr());

            let p = BytePage::from("123");
            assert_eq!(p.len(), 3);
            assert!(!p.is_empty());
            assert_eq!(p.as_ref(), b"123");
            assert_eq!(p.as_ref().as_ptr(), p.as_ptr());
            assert_eq!(p.freeze(), b"123");

            let p = BytePage::from(vec![b'1', b'2', b'3']);
            assert_eq!(p.len(), 3);
            assert!(!p.is_empty());
            assert_eq!(p.as_ref(), b"123");
            assert_eq!(p.as_ref().as_ptr(), p.as_ptr());
            assert_eq!(p.freeze(), b"123");

            let mut p = BytePage::from(vec![b'1', b'2', b'3']);
            p.advance_to(1);
            assert_eq!(p.len(), 2);
            assert!(!p.is_empty());
            assert_eq!(p.as_ref(), b"23");

            // debug
            let mut pages = BytePages::new(BytePageSize::Size8);
            pages.extend_from_slice(b"b");
            assert_eq!(format!("{pages:?}"), "BytePages(b\"b\")");
            let p = pages.take().unwrap();
            assert_eq!(p.as_ref(), b"b");

            let mut pages = BytePages::new(BytePageSize::Size8);
            pages.extend_from_slice(b"a");
            pages.append(Bytes::copy_from_slice(b"123"));
            pages.push_back(p);
            assert_eq!(format!("{pages:?}"), "BytePages(b\"b\", b\"a123\")");

            assert_eq!(pages.len(), 5);
            pages.clear();
            assert_eq!(pages.len(), 0);
        }
    }

    /// Checks the tracked length against the pages.
    fn assert_len(pages: &BytePages) {
        let len = pages.pages().iter().map(BytePage::len).sum::<usize>() + pages.current_len();
        assert_eq!(pages.len(), len);
        assert_eq!(pages.is_empty(), len == 0);
    }

    #[test]
    fn pages_len_tracking() {
        let mut rng = rand::rng();
        let mut pages = BytePages::new(BytePageSize::Size8);
        let mut other = BytePages::new(BytePageSize::Size8);
        let mut expected = 0;

        let (iters, max) = if cfg!(miri) { (100, 256) } else { (2000, 12 * 1024) };
        for _ in 0..iters {
            let n = rng.random_range(0..max);
            match rng.random_range(0..9) {
                0 => {
                    pages.extend_from_slice(&vec![1; n]);
                    expected += n;
                }
                1 => {
                    pages.append(Bytes::copy_from_slice(&vec![2; n]));
                    expected += n;
                }
                2 => {
                    if pages.prepend(BytesMut::copy_from_slice(vec![3; n])) {
                        expected += n;
                    }
                }
                3 => {
                    if let Some(p) = pages.take() {
                        expected -= p.len();
                    }
                }
                4 => {
                    let at = cmp::min(n, expected);
                    let split = pages.split_to(at);
                    assert_len(&split);
                    assert_eq!(split.len(), at);
                    expected -= at;
                }
                5 => {
                    let at = cmp::min(n, expected);
                    pages.split_into(at, &mut other);
                    expected -= at;
                }
                6 => {
                    other.move_to(&mut pages);
                    assert_len(&other);
                    assert!(other.is_empty());
                    expected = pages.len();
                }
                7 => {
                    let mut copy = BytePages::new(BytePageSize::Size8);
                    pages.copy_to(&mut copy);
                    assert_len(&copy);
                    assert_eq!(copy.len(), expected);
                }
                _ => {
                    pages.put_u8(4);
                    expected += 1;
                }
            }
            assert_len(&pages);
            assert_len(&other);
            assert_eq!(pages.len(), expected);
        }

        let len = pages.len();
        assert_eq!(pages.freeze().len(), len);
        assert_len(&pages);
        assert!(pages.is_empty());
        pages.extend_from_slice(b"123");
        pages.clear();
        assert_len(&pages);
        assert!(pages.is_empty());
    }

    #[test]
    fn pages_len_after_cache_reuse() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.append(Bytes::copy_from_slice(&[1; 64]));
        pages.append(Bytes::copy_from_slice(&[1; 64]));
        drop(pages);

        let pages = BytePages::new(BytePageSize::Size8);
        assert_len(&pages);
        assert!(pages.is_empty());
    }

    #[test]
    fn pages_copy_to_shares_current() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        let ptr = unsafe { pages.current.as_ref().unwrap().as_ptr() };

        let mut copy = BytePages::new(BytePageSize::Size8);
        pages.copy_to(&mut copy);
        let page = copy.take().unwrap();
        assert_eq!(unsafe { page.as_ptr() }, ptr.cast_const());
        assert_eq!(page.as_ref(), &[1; 64][..]);

        // the source keeps writing to the page it shares with the copy
        pages.put_slice(&[2; 64]);
        assert_eq!(unsafe { pages.current.as_ref().unwrap().as_ptr() }, ptr);
        assert_eq!(page.as_ref(), &[1; 64][..]);
        assert_eq!(pages.len(), 128);

        drop(pages);
        assert_eq!(page.as_ref(), &[1; 64][..]);
    }

    #[test]
    fn pages_split_keeps_current_writable() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        let ptr = unsafe { pages.current.as_ref().unwrap().as_ptr() };
        let remaining = pages.spare();

        let mut head = pages.split_to(40);
        assert_eq!(head.len(), 40);
        assert_eq!(pages.len(), 24);
        assert_eq!(pages.num_pages(), 1);
        assert_eq!(pages.spare(), remaining);

        // new data goes into the same page
        pages.put_slice(&[2; 16]);
        assert_eq!(pages.num_pages(), 1);
        assert_eq!(
            unsafe { pages.current.as_ref().unwrap().as_ptr() },
            ptr.wrapping_add(40)
        );
        let mut expected = vec![1; 24];
        expected.extend_from_slice(&[2; 16]);
        assert_eq!(&pages.freeze()[..], &expected[..]);
        assert_eq!(&head.freeze()[..], &[1; 40][..]);

        // an inline head
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        let mut to = BytePages::new(BytePageSize::Size8);
        pages.split_into(2, &mut to);
        assert_eq!(&to.freeze()[..], &[1; 2][..]);
        assert_eq!(pages.spare(), remaining);
        assert_eq!(pages.len(), 62);

        // the whole current page moves with its spare capacity
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        let ptr = unsafe { pages.current.as_ref().unwrap().as_ptr() };
        let to = pages.split_to(64);
        assert!(pages.is_empty());
        assert_eq!(pages.num_pages(), 0);
        assert_eq!(to.spare(), remaining);
        assert_eq!(unsafe { to.current.as_ref().unwrap().as_ptr() }, ptr);

        // an empty current page stays in place
        let mut pages = BytePages::new(BytePageSize::Size8);
        let _ = pages.chunk_mut();
        let head = pages.split_to(10);
        assert!(head.is_empty());
        assert_eq!(pages.spare(), remaining + 64);
    }

    #[test]
    fn pages_buf_mut_grows() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        assert!(pages.has_remaining_mut());
        assert_eq!(pages.remaining_mut(), usize::MAX);
        unsafe { pages.advance_mut(0) };

        // filling the current page through `chunk_mut` starts a new one
        let n = pages.chunk_mut().len();
        unsafe {
            std::ptr::write_bytes(pages.chunk_mut().as_mut_ptr(), 1, n);
            pages.advance_mut(n);
        }
        assert!(pages.chunk_mut().len() > 0);
        // the new current page holds no data yet
        assert_eq!(pages.num_pages(), 1);
        unsafe {
            *pages.chunk_mut().as_mut_ptr() = 2;
            pages.advance_mut(1);
        }
        assert_eq!(pages.len(), n + 1);
        assert_eq!(pages.remaining_mut(), usize::MAX - n - 1);

        // `put` and `io::Write` are not limited to the current page
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put(&[3u8; 200][..]);
        pages.put(Bytes::from_static(b"abcd"));
        io::Write::write_all(&mut pages, &[4; 100]).unwrap();
        let mut expected = vec![3; 200];
        expected.extend_from_slice(b"abcd");
        expected.extend_from_slice(&[4; 100]);
        assert_eq!(&pages.freeze()[..], &expected[..]);
    }

    #[test]
    #[should_panic(expected = "cannot advance past the current page")]
    fn pages_advance_past_current_page() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        let n = pages.chunk_mut().len();
        unsafe { pages.advance_mut(n + 1) };
    }

    #[test]
    #[should_panic(expected = "Page size cannot be Unset")]
    fn pages_new_unset() {
        let _ = BytePages::new(BytePageSize::Unset);
    }

    #[test]
    #[should_panic(expected = "Page size cannot be Unset")]
    fn pages_set_page_size_unset() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.set_page_size(BytePageSize::Unset);
    }

    #[test]
    fn cached_pages_list_is_shrunk() {
        let mut pages = BytePages::new(BytePageSize::Size4);
        for _ in 0..200 {
            pages.append(Bytes::from_static(b"page"));
        }
        assert!(pages.pages().capacity() >= 200);
        drop(pages);

        let pages = BytePages::new(BytePageSize::Size4);
        assert!(
            pages.pages().capacity() < 200,
            "{}",
            pages.pages().capacity()
        );
    }

    #[test]
    fn pages_into_bytes_single_page() {
        // the current page
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        let ptr = unsafe { pages.current.as_ref().unwrap().as_ptr() };
        let mut buf = BytesMut::from(pages);
        assert_eq!(buf.as_ptr(), ptr.cast_const());
        assert_eq!(&buf[..], &[1; 64][..]);
        // the rest of the page is spare capacity
        assert_eq!(buf.capacity(), BytePageSize::Size8.capacity());
        buf.extend_from_slice(&[2; 64]);
        assert_eq!(buf.as_ptr(), ptr.cast_const());

        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        let ptr = unsafe { pages.current.as_ref().unwrap().as_ptr() };
        let b = Bytes::from(pages);
        assert_eq!(b.as_ptr(), ptr.cast_const());
        assert_eq!(&b[..], &[1; 64][..]);

        // a `Bytes` page
        let src = Bytes::copy_from_slice(&[3; 64]);
        let ptr = src.as_ptr();
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.prepend(src);
        assert_eq!(BytesMut::from(pages).as_ptr(), ptr);

        // a shared page is copied
        let src = Bytes::copy_from_slice(&[3; 64]);
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.prepend(&src);
        let mut buf = BytesMut::from(pages);
        assert_ne!(buf.as_ptr(), src.as_ptr());
        buf[0] = 4;
        assert_eq!(&src[..], &[3; 64][..]);
    }

    #[test]
    fn pages_into_bytes_multiple_pages() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.prepend(Bytes::copy_from_slice(&[1; 64]));
        pages.put_slice(&[2; 64]);
        assert_eq!(pages.num_pages(), 2);
        let buf = BytesMut::from(pages);
        assert_eq!(&buf[..64], &[1; 64][..]);
        assert_eq!(&buf[64..], &[2; 64][..]);

        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.prepend(Bytes::copy_from_slice(&[1; 64]));
        pages.put_slice(&[2; 64]);
        let b = Bytes::from(pages);
        assert_eq!(&b[..64], &[1; 64][..]);
        assert_eq!(&b[64..], &[2; 64][..]);

        assert!(BytesMut::from(BytePages::new(BytePageSize::Size8)).is_empty());
        assert!(Bytes::from(BytePages::new(BytePageSize::Size8)).is_empty());
    }

    #[test]
    fn pages_copy_to() {
        let mut pages = BytePages::default();
        let mut pages2 = BytePages::default();
        pages.put_slice(b"456");
        pages.prepend(BytePage::from(Bytes::copy_from_slice(b"123")));
        pages.copy_to(&mut pages2);
        let p = pages.freeze();
        assert_eq!(p, b"123456");
        let p2 = pages2.freeze();
        assert_eq!(p2, b"123456");

        let mut pages = BytePages::default();
        let mut pages2 = BytePages::default();
        pages.put_slice(b"456");
        pages.prepend(BytePage::from(Bytes::copy_from_slice(b"123")));
        pages.copy_to(&mut pages2);
        pages.put_u8(b'7');
        let p = pages.freeze();
        assert_eq!(p, b"1234567");
        let p2 = pages2.freeze();
        assert_eq!(p2, b"123456");

        let mut pages = BytePages::default();
        pages.put_slice(b"456");
        pages.prepend(BytePage::from(Bytes::copy_from_slice(b"123")));
        let mut pages2 = pages.clone();
        pages.put_u8(b'7');
        let p = pages.freeze();
        assert_eq!(p, b"1234567");
        let p2 = pages2.freeze();
        assert_eq!(p2, b"123456");
    }

    #[test]
    fn pages_methods() {
        // .split_to()
        let mut pages = BytePages::default();
        pages.put_slice(b"456");
        pages.prepend(BytePage::from(&Bytes::copy_from_slice(b"123")));
        let mut pages2 = pages.split_to(1);
        let p = pages.freeze();
        assert_eq!(p, b"23456");
        let p2 = pages2.freeze();
        assert_eq!(p2, b"1");

        let mut pages = BytePages::default();
        pages.put_slice(b"456");
        pages.prepend(BytePage::from(Bytes::copy_from_slice(b"123")));
        let mut pages2 = pages.split_to(4);
        let p = pages.freeze();
        assert_eq!(p, b"56");
        let p2 = pages2.freeze();
        assert_eq!(p2, b"1234");

        // .split_into()
        let mut pages = BytePages::default();
        pages.put_slice(b"456");
        pages.prepend(BytePage::from(crate::ByteString::from_static("123")));
        let mut pages2 = BytePages::default();
        pages.split_into(1, &mut pages2);
        let p = pages.freeze();
        assert_eq!(p, b"23456");
        let p2 = pages2.freeze();
        assert_eq!(p2, b"1");

        // .with_bytes_mut()
        let mut pages = BytePages::default();
        #[allow(deprecated)]
        pages.with_bytes_mut(|buf| buf.extend_from_slice(b"123"));
        assert_eq!(pages.len(), 3);
        let p = pages.freeze();
        assert_eq!(p, b"123");

        let data = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(65_536)
            .map(char::from)
            .collect::<String>();

        let mut pages = BytePages::default();
        #[allow(deprecated)]
        pages.with_bytes_mut(|buf| buf.extend_from_slice(data.as_bytes()));
        assert_eq!(pages.len(), 65_536);
        let p = pages.freeze();
        assert_eq!(p, data.as_bytes());

        // into bytes
        let page = BytePage::from(Bytes::copy_from_slice(b"123"));
        assert_eq!(page, b"123");
        assert_eq!(<BytePage as Borrow<[u8]>>::borrow(&page), b"123");
        let b = Bytes::from(page);
        assert_eq!(b, b"123");
    }

    #[test]
    fn page_clone() {
        // Bytes storage
        let p = BytePage::from(Bytes::copy_from_slice(b"123"));
        let p2 = p.clone();
        assert_eq!(p, p2);

        // StorageVec
        let mut p = BytePage::from(BytesMut::copy_from_slice(b"123"));
        if let StorageType::Storage(ref mut st) = p.inner {
            assert!(st.is_unique());
        } else {
            panic!()
        }
        let p2 = p.clone();
        assert_eq!(p, p2);
        // short data is copied into an inline view
        assert!(matches!(p2.inner, StorageType::Bytes(_)));
        if let StorageType::Storage(st) = p.inner {
            assert!(st.is_unique());
        } else {
            panic!()
        }

        let mut p = BytePage::from(BytesMut::copy_from_slice([b'1'; 64]));
        let p2 = p.clone();
        assert_eq!(p, p2);
        assert!(matches!(p2.inner, StorageType::Bytes(_)));
        if let StorageType::Storage(ref mut st) = p.inner {
            assert!(!st.is_unique());
        } else {
            panic!()
        }
        drop(p2);
        if let StorageType::Storage(st) = p.inner {
            assert!(st.is_unique());
        } else {
            panic!()
        }

        // Vec<u8> storage
        let p = BytePage::from(vec![b'1', b'2', b'3']);
        let p2 = p.clone();
        assert_eq!(p, p2);
        if let StorageType::Bytes(_) = p2.inner {
        } else {
            panic!()
        }
    }

    #[test]
    fn page_split_to() {
        // Bytes storage
        let mut p = BytePage::from(Bytes::copy_from_slice(b"123"));
        let p2 = p.split_to(1);
        assert_eq!(p, b"23");
        assert_eq!(p2, b"1");

        // StorageVec
        let mut p = BytePage::from(BytesMut::copy_from_slice(b"123"));
        let p2 = p.split_to(1);
        assert_eq!(p, b"23");
        assert_eq!(p2, b"1");

        // Vec<u8> storage
        let mut p = BytePage::from(vec![b'1', b'2', b'3']);
        let p2 = p.split_to(1);
        assert_eq!(p, b"23");
        assert_eq!(p2, b"1");
    }

    #[test]
    fn page_read() {
        use std::io::Read;

        let mut page = BytePage::from(Bytes::copy_from_slice(b"123"));

        let mut buf = [0; 10];
        assert_eq!(page.read(&mut buf).unwrap(), 3);
        assert_eq!(page.len(), 0);
        assert_eq!(buf, [49, 50, 51, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn pages_misc() {
        let mut pages = BytePages::new(BytePageSize::Size4);
        pages.set_page_size(BytePageSize::Size8);
        assert_eq!(pages.page_size(), BytePageSize::Size8);
        assert!(!pages.prepend(Bytes::new()));
        assert!(pages.prepend(Bytes::from_static(b"a")));

        io::Write::write_all(&mut pages, b"bc").unwrap();
        io::Write::flush(&mut pages).unwrap();
        assert_eq!(pages.freeze(), "abc");

        let s = ByteString::from_static("str");
        pages.append(&s);
        assert_eq!(pages.freeze(), "str");
    }

    #[test]
    fn pages_try_get_current_from() {
        let mut src = BytePages::new(BytePageSize::Size4);
        src.extend_from_slice(b"data");

        // the target already holds data
        let mut dst = BytePages::new(BytePageSize::Size4);
        dst.append(Bytes::from_static(b"x"));
        dst.try_get_current_from(&mut src);
        assert_eq!(src.len(), 4);
        assert_eq!(dst.len(), 1);

        let mut dst = BytePages::new(BytePageSize::Size4);
        dst.try_get_current_from(&mut src);
        assert_eq!(src.len(), 0);
        assert_eq!(dst.len(), 4);
        dst.extend_from_slice(b"!");
        assert_eq!(dst.freeze(), "data!");
    }

    #[test]
    fn pages_drop_cache() {
        CACHE.with(|c| c.set(Some(Box::default())));
        let cached = || {
            CACHE.with(|c| {
                let cache = c.take().unwrap();
                let len = cache.len();
                c.set(Some(cache));
                len
            })
        };

        // the cache is full
        let pages: Vec<_> = (0..=CACHE_SIZE)
            .map(|_| BytePages::new(BytePageSize::Size4))
            .collect();
        drop(pages);
        assert_eq!(cached(), CACHE_SIZE);

        // the cache is in use
        CACHE.with(|c| c.set(Some(Box::default())));
        let pages = BytePages::new(BytePageSize::Size4);
        let cache = CACHE.with(Cell::take);
        drop(pages);
        CACHE.with(|c| c.set(cache));
        assert_eq!(cached(), 0);
    }

    #[test]
    fn page_conversions() {
        let page = BytePage::from(BytesMut::copy_from_slice([1; 64]));
        assert_eq!(page.info(), crate::info::PageKind::Storage);
        assert_eq!(Bytes::from(page), &[1; 64][..]);

        let page = BytePage::from(vec![2; 64]);
        assert_eq!(page.info(), crate::info::PageKind::Vec);
        assert_eq!(Bytes::from(page), &[2; 64][..]);

        let page = BytePage::from(vec![3; 64]);
        assert_eq!(BytesMut::from(page), &[3; 64][..]);

        let s = ByteString::from_static("string");
        assert_eq!(BytePage::from(&s), "string");
    }

    #[test]
    fn append_storage_page() {
        // a page with spare capacity becomes the current page
        let mut pages = BytePages::new(BytePageSize::Size4);
        let mut buf = BytesMut::with_capacity(64);
        buf.extend_from_slice(b"a");
        pages.append(buf);
        pages.extend_from_slice(b"b");
        assert_eq!(pages.num_pages(), 1);
        assert_eq!(pages.freeze(), "ab");

        // full and shared pages are added to the page list
        let mut buf = BytesMut::with_capacity(64);
        let cap = buf.capacity();
        buf.resize(cap, b'x');
        pages.append(buf);
        let page = BytePage::from(BytesMut::copy_from_slice([b'y'; 64]));
        let shared = page.clone();
        pages.append(page);
        pages.extend_from_slice(b"z");
        assert_eq!(pages.num_pages(), 3);
        assert_eq!(pages.len(), cap + 65);
        assert_eq!(shared, &[b'y'; 64][..]);
    }
}
