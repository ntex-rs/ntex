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
    /// Pages are allocated lazily using the specified capacity category. Like
    /// [`BytesMut::with_page_size`], a new page can be a cached page of the
    /// next smaller size.
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
        st.pages.push_back(page);
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
                // the spare capacity of the current page is kept
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

    /// Returns an iterator over the buffered data, one slice per page.
    ///
    /// The current writable page is yielded last, if it holds data. The
    /// pages are stored on the heap, so their data stays at the same address
    /// when `self` is moved, until `self` is modified or dropped.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        self.pages().iter().map(BytePage::as_ref).chain(
            self.current
                .as_ref()
                .map(StorageVec::as_ref)
                .filter(|s| !s.is_empty()),
        )
    }

    /// Advance the internal cursor.
    ///
    /// Afterwards `self` contains elements `[cnt, len)`. Fully consumed pages
    /// are dropped, the current writable page keeps its spare capacity.
    ///
    /// # Panics
    ///
    /// Panics if `cnt > len`.
    pub fn advance_to(&mut self, mut cnt: usize) {
        assert!(
            cnt <= self.len(),
            "cannot advance past the end of the buffer, cnt:{cnt} len:{}",
            self.len()
        );

        while cnt > 0
            && let Some(mut page) = self.pop_front()
        {
            if cnt < page.len() {
                page.advance_to(cnt);
                self.push_front(page);
                return;
            }
            cnt -= page.len();
        }
        if cnt > 0
            && let Some(st) = self.current.as_mut()
        {
            // SAFETY: `cnt <= st.len()`, checked by the assert above
            unsafe { st.set_start(cnt) };
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

    /// Moves all buffered data to the back of another [`BytePages`] value,
    /// leaving `self` empty.
    ///
    /// Pages are moved as is, without copying or merging. The filled part of
    /// the current page of `pages` is split off in front of the moved data,
    /// its spare capacity stays available for later writes. The current page
    /// of `self` is moved as a regular page.
    pub fn move_to(&mut self, pages: &mut BytePages) {
        if self.is_empty() {
            return;
        }

        if let Some(st) = pages.current.as_mut()
            && st.len() != 0
        {
            let head = st.split_to(st.len());
            pages.push_back(<BytePage as From<Bytes>>::from(Bytes { storage: head }));
        }

        let src = self.st.as_mut().unwrap();
        let dst = pages.st.as_mut().unwrap();
        if dst.pages.is_empty() {
            mem::swap(&mut src.pages, &mut dst.pages);
            dst.len = mem::take(&mut src.len);
        } else {
            while let Some(page) = self.pop_front() {
                pages.push_back(page);
            }
        }

        if self.current_len() != 0
            && let Some(st) = self.current.take()
        {
            pages.push_back(BytePage::from(st));
        }
    }

    /// Splits the buffer into two at the given index.
    ///
    /// Afterwards, `self` contains elements `[at, len)`, and the returned [`BytePages`]
    /// contains elements `[0, at)`. If `at > len`, all data is moved.
    ///
    /// Unlike [`Bytes::split_to`] and [`BytesMut::split_to`](crate::BytesMut::split_to),
    /// which panic when `at` is out of bounds, this clamps `at` to the length.
    /// Check `len()` first if a short split is an error.
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
    /// Like [`split_to`](Self::split_to), this clamps `at` to the length instead
    /// of panicking.
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

    /// Passes a new mutable buffer to `f` and appends its contents.
    ///
    /// After `f` returns, the buffer is appended according to the rules of
    /// [`append`](Self::append). Existing pages and the current writable page
    /// are not exposed to `f`. If `f` panics, the temporary buffer is dropped
    /// and this collection is unchanged.
    ///
    /// This is a low-level API intended for ntex internals and may change
    /// without notice.
    #[doc(hidden)]
    #[deprecated(
        since = "1.10.0",
        note = "use the `BufMut` methods of `BytePages` instead"
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

    // Returns the current page if it has spare capacity, or a new page.
    //
    // The current page can be full, for example after `advance_mut` filled
    // it, a full page moves to the page list, an empty one is dropped.
    fn take_writable(&mut self) -> StorageVec {
        match self.current.take() {
            Some(st) if !st.is_full() => st,
            Some(st) => {
                if st.len() != 0 {
                    self.push_back(BytePage::from(st));
                }
                StorageVec::sized(self.page_size())
            }
            None => StorageVec::sized(self.page_size()),
        }
    }

    fn with_current<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut StorageVec) -> R,
    {
        let mut st = self.take_writable();
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
        let st = self.take_writable();
        self.current.insert(st).spare_mut()
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

    #[inline]
    fn put_slice(&mut self, src: &[u8]) {
        // a small write that leaves the current page non-full is copied in place
        if let Some(st) = self.current.as_mut()
            && st.remaining() > src.len()
        {
            st.put_slice_partial(src);
        } else {
            self.put_slice_pages(src);
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

impl BytePages {
    fn put_slice_pages(&mut self, mut src: &[u8]) {
        while !src.is_empty() {
            let amount = self.with_current(|st| st.put_slice_partial(src));

            src = &src[amount..];
        }
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
        // SAFETY: Each variant keeps its backing storage alive; the caller must preserve the page
        // while using the pointer.
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
    /// Unlike [`Bytes::split_to`] and [`BytesMut::split_to`](crate::BytesMut::split_to),
    /// which panic when `at` is out of bounds, this clamps `at` to the length.
    /// Check `len()` first if a short split is an error.
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
            // SAFETY: `set_start` checks `cnt <= len` before advancing within the live, exclusively
            // owned view.
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
            // Converting back to `StorageVec` requires uniqueness.
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

    /// STATEMENT: `BytePage` compares by content across byte and string types and supports consuming reads.
    ///
    /// HOW:
    /// - Compare a vector-backed `hello` page with borrowed and owned equivalents in both directions and reject `hell`.
    /// - Read it into a three-byte array, checking `hel`, the `lo` remainder, short and exhausted reads, and final emptiness.
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

    /// STATEMENT: Page metadata distinguishes bytes, mutable storage, vector storage, and inline bytes.
    ///
    /// HOW:
    /// - Convert 64-byte `Bytes`, `BytesMut`, and vectors and check their `PageKind` values.
    /// - Compare inline status for four-byte and 64-byte immutable buffers and a short mutable buffer.
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

    /// STATEMENT: Appending small data copies into the current page while large data retains its own allocation.
    ///
    /// HOW:
    /// - Append payloads at and above `APPEND_COPY_LIMIT` after a header and check page counts, spare capacity, and storage pointers.
    /// - Add a trailer, drain pages in order, compare concatenated contents, and verify an empty current page remains reusable.
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

    /// STATEMENT: Pages track lengths, page boundaries, conversions, and debug output as data is added and removed.
    ///
    /// HOW:
    /// - Fill `Size8` buffers across page boundaries, append a full page, and check lengths, page counts, spare capacity, and drained contents.
    /// - Convert byte and string inputs to pages, advance and freeze them, then check debug ordering and clearing.
    #[test]
    fn pages() {
        let cap = BytePageSize::Size8.capacity();
        // SAFETY: Each `as_ptr` result is compared immediately while its page remains live,
        // unmoved, and unchanged.
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

    /// STATEMENT: Tracked total length stays consistent with page contents across mixed buffer operations.
    ///
    /// HOW:
    /// - Randomly extend, append, prepend, take, split, move, copy, and write bytes while maintaining an expected length.
    /// - Check both buffers against their constituent page lengths after each operation, then verify freezing and clearing leave empty buffers.
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

    /// STATEMENT: A cached page-list allocation does not retain the previous buffer's tracked length.
    ///
    /// HOW:
    /// - Append two 64-byte buffers, drop the `BytePages`, then create another and verify empty length and page consistency.
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

    /// STATEMENT: Copying the current page shares its data while allowing the source to append safely.
    ///
    /// HOW:
    /// - Copy 64 bytes with `copy_to`, take the copied page, and verify it shares the current page's pointer.
    /// - Append another 64 bytes to the source and drop it, checking the copied page stays unchanged and valid.
    #[test]
    fn pages_copy_to_shares_current() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        // SAFETY: The current page owns a live allocation; the pointer is only recorded for
        // comparison.
        let ptr = unsafe { pages.current.as_ref().unwrap().as_ptr() };

        let mut copy = BytePages::new(BytePageSize::Size8);
        pages.copy_to(&mut copy);
        let page = copy.take().unwrap();
        // SAFETY: The page is live and unmoved during this pointer comparison.
        assert_eq!(unsafe { page.as_ptr() }, ptr.cast_const());
        assert_eq!(page.as_ref(), &[1; 64][..]);

        // the source keeps writing to the page it shares with the copy
        pages.put_slice(&[2; 64]);
        // SAFETY: The current page owns a live allocation; the pointer is only compared, not
        // dereferenced.
        assert_eq!(unsafe { pages.current.as_ref().unwrap().as_ptr() }, ptr);
        assert_eq!(page.as_ref(), &[1; 64][..]);
        assert_eq!(pages.len(), 128);

        drop(pages);
        assert_eq!(page.as_ref(), &[1; 64][..]);
    }

    /// STATEMENT: Splitting pages preserves writable spare capacity and transfers a whole current page without copying.
    ///
    /// HOW:
    /// - Split a 40-byte head from 64 bytes, append to the tail, and check lengths, contents, spare capacity, and pointer offsets.
    /// - Repeat with an inline head, a whole-page split, and an empty current page, checking ownership and retained capacity.
    #[test]
    fn pages_split_keeps_current_writable() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        // SAFETY: The current page owns a live allocation; the pointer is only recorded for
        // comparison.
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
            // SAFETY: The current page owns a live allocation; the pointer is only compared, not
            // dereferenced.
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
        // SAFETY: The current page owns a live allocation; the pointer is only recorded for
        // comparison.
        let ptr = unsafe { pages.current.as_ref().unwrap().as_ptr() };
        let to = pages.split_to(64);
        assert!(pages.is_empty());
        assert_eq!(pages.num_pages(), 0);
        assert_eq!(to.spare(), remaining);
        // SAFETY: The destination owns the moved allocation; its pointer is only compared, not
        // dereferenced.
        assert_eq!(unsafe { to.current.as_ref().unwrap().as_ptr() }, ptr);

        // an empty current page stays in place
        let mut pages = BytePages::new(BytePageSize::Size8);
        let _ = pages.chunk_mut();
        let head = pages.split_to(10);
        assert!(head.is_empty());
        assert_eq!(pages.spare(), remaining + 64);
    }

    /// STATEMENT: `BytePages` mutable-buffer writes grow across page boundaries and account for all written bytes.
    ///
    /// HOW:
    /// - Fill a writable chunk, advance it, write into the next page, and check page counts, length, and remaining writable space.
    /// - Combine `put` and `Write::write_all`, then freeze and compare the complete payload.
    #[test]
    fn pages_buf_mut_grows() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        assert!(pages.has_remaining_mut());
        assert_eq!(pages.remaining_mut(), usize::MAX);
        // SAFETY: Advancing by zero exposes no uninitialized bytes.
        unsafe { pages.advance_mut(0) };

        // filling the current page through `chunk_mut` starts a new one
        let n = pages.chunk_mut().len();
        // SAFETY: The writable chunk has `n` bytes, all initialized by `write_bytes` before
        // advancing.
        unsafe {
            std::ptr::write_bytes(pages.chunk_mut().as_mut_ptr(), 1, n);
            pages.advance_mut(n);
        }
        assert!(pages.chunk_mut().len() > 0);
        // the new current page holds no data yet
        assert_eq!(pages.num_pages(), 1);
        // SAFETY: The nonempty writable chunk receives one initialized byte before advancing by
        // one.
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

    /// STATEMENT: Single-byte writes after a full current page allocate usable storage without adding empty pages.
    ///
    /// HOW:
    /// - Fill a current page through `advance_mut`, append signed and unsigned bytes, and check page counts and frozen data.
    /// - Repeat after appending a large separate payload, verifying all listed pages are nonempty and byte order is preserved.
    #[test]
    fn pages_byte_write_after_full_page() {
        // `advance_mut` leaves a full current page
        let mut pages = BytePages::new(BytePageSize::Size8);
        let n = pages.chunk_mut().len();
        // SAFETY: The writable chunk has `n` bytes, all initialized by `write_bytes` before
        // advancing.
        unsafe {
            std::ptr::write_bytes(pages.chunk_mut().as_mut_ptr(), 1, n);
            pages.advance_mut(n);
        }
        pages.put_u8(2);
        pages.put_i8(3);
        assert_eq!(pages.num_pages(), 2);
        assert_eq!(pages.pages()[0].len(), n);
        let mut expected = vec![1; n];
        expected.extend_from_slice(&[2, 3]);
        assert_eq!(&pages.freeze()[..], &expected[..]);

        // splitting off a full current page leaves an empty page without capacity
        let mut pages = BytePages::new(BytePageSize::Size8);
        let n = pages.chunk_mut().len();
        // SAFETY: The writable chunk has `n` bytes, all initialized by `write_bytes` before
        // advancing.
        unsafe {
            std::ptr::write_bytes(pages.chunk_mut().as_mut_ptr(), 1, n);
            pages.advance_mut(n);
        }
        pages.append(vec![2; APPEND_COPY_LIMIT + 1]);
        pages.put_u8(3);
        assert!(pages.pages().iter().all(|p| !p.is_empty()));
        let mut expected = vec![1; n];
        expected.extend_from_slice(&[2; APPEND_COPY_LIMIT + 1]);
        expected.push(3);
        assert_eq!(&pages.freeze()[..], &expected[..]);
    }

    /// STATEMENT: Mutable advancement cannot exceed the current page's writable chunk.
    ///
    /// HOW:
    /// - Get a `Size8` buffer's chunk length, advance by one more, and expect the current-page bounds panic.
    #[test]
    #[should_panic(expected = "cannot advance past the current page")]
    fn pages_advance_past_current_page() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        let n = pages.chunk_mut().len();
        // SAFETY: The implementation checks the chunk bound and panics before exposing any
        // uninitialized bytes.
        unsafe { pages.advance_mut(n + 1) };
    }

    /// STATEMENT: Creating `BytePages` with an unspecified page size is rejected.
    ///
    /// HOW:
    /// - Call `BytePages::new` with `BytePageSize::Unset` and expect the page-size panic.
    #[test]
    #[should_panic(expected = "Page size cannot be Unset")]
    fn pages_new_unset() {
        let _ = BytePages::new(BytePageSize::Unset);
    }

    /// STATEMENT: An existing `BytePages` buffer cannot switch to an unspecified page size.
    ///
    /// HOW:
    /// - Create a `Size8` buffer, set its page size to `Unset`, and expect the page-size panic.
    #[test]
    #[should_panic(expected = "Page size cannot be Unset")]
    fn pages_set_page_size_unset() {
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.set_page_size(BytePageSize::Unset);
    }

    /// STATEMENT: Caching a large page list does not retain its excessive list capacity.
    ///
    /// HOW:
    /// - Append 200 pages, check list capacity is at least 200, drop the buffer, and verify a new list has smaller capacity.
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

    /// STATEMENT: Single-page conversion reuses unique storage but copies shared storage before making it mutable.
    ///
    /// HOW:
    /// - Convert current pages to `BytesMut` and `Bytes`, checking pointer reuse, contents, and writable spare capacity.
    /// - Convert unique and shared prepended `Bytes`, checking reuse only for the unique case and source isolation after mutation.
    #[test]
    fn pages_into_bytes_single_page() {
        // the current page
        let mut pages = BytePages::new(BytePageSize::Size8);
        pages.put_slice(&[1; 64]);
        // SAFETY: The current page owns a live allocation; the pointer is only recorded for
        // comparison.
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
        // SAFETY: The current page owns a live allocation; the pointer is only recorded for
        // comparison.
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

    /// STATEMENT: Converting multiple pages concatenates them in order and converting an empty buffer stays empty.
    ///
    /// HOW:
    /// - Prepend 64 ones and write 64 twos, convert separately to `BytesMut` and `Bytes`, and check each half.
    /// - Convert empty `Size8` buffers to both destination types and verify emptiness.
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

    /// STATEMENT: Copied and cloned page buffers retain their original contents when the source grows.
    ///
    /// HOW:
    /// - Copy pages containing `123456`, freeze both, and compare their contents.
    /// - Repeat with `copy_to` and `clone`, append `7` only to the source, and verify each destination remains `123456`.
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

    /// STATEMENT: Page splitting, mutable access, borrowing, and byte conversion preserve the requested data ranges.
    ///
    /// HOW:
    /// - Split `123456` within and across pages with `split_to` and `split_into` and check both resulting buffers.
    /// - Write short and 65,536-byte payloads through `with_bytes_mut`, then check lengths, frozen bytes, and page borrowing/conversion.
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

        let data = b"0123456789abcdef".repeat(4096);

        let mut pages = BytePages::default();
        #[allow(deprecated)]
        pages.with_bytes_mut(|buf| buf.extend_from_slice(&data));
        assert_eq!(pages.len(), 65_536);
        let p = pages.freeze();
        assert_eq!(p, data[..]);

        // into bytes
        let page = BytePage::from(Bytes::copy_from_slice(b"123"));
        assert_eq!(page, b"123");
        assert_eq!(<BytePage as Borrow<[u8]>>::borrow(&page), b"123");
        let b = Bytes::from(page);
        assert_eq!(b, b"123");
    }

    /// STATEMENT: A slice written into the current page stays there until it fills the page, which then moves to the page list.
    ///
    /// HOW:
    /// - Write slices that leave one spare byte in the first page and check that no page was pushed.
    /// - Write the last byte and check that the full page is pushed with no current page left.
    /// - Write a slice larger than the page and check the page count, length, and frozen bytes.
    #[test]
    fn put_slice_current_page() {
        let mut pages = BytePages::new(BytePageSize::Size4);
        pages.put_slice(b"abc");
        let spare = pages.spare();
        assert!(spare > 1);
        assert_eq!(pages.pages().len(), 0);
        assert_eq!(pages.num_pages(), 1);

        let fill = vec![b'x'; spare - 1];
        pages.put_slice(&fill);
        assert_eq!(pages.spare(), 1);
        assert_eq!(pages.pages().len(), 0);

        pages.put_slice(b"y");
        assert_eq!(pages.pages().len(), 1);
        assert!(pages.current.is_none());
        assert_eq!(pages.num_pages(), 1);
        let page_len = pages.len();
        assert_eq!(page_len, spare + 3);

        let large = vec![b'z'; page_len + 10];
        pages.put_slice(&large);
        assert_eq!(pages.len(), page_len * 2 + 10);
        assert_eq!(pages.num_pages(), 3);

        let mut expected = b"abc".to_vec();
        expected.extend_from_slice(&fill);
        expected.push(b'y');
        expected.extend_from_slice(&large);
        assert_eq!(pages.freeze(), expected[..]);
    }

    /// STATEMENT: Cloned pages preserve contents while short storage is copied inline and longer mutable storage is shared.
    ///
    /// HOW:
    /// - Clone `Bytes`, short and long `BytesMut`, and vector-backed pages, checking equal contents and resulting storage variants.
    /// - Inspect mutable-storage uniqueness before and after cloning and dropping the longer clone.
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

    /// STATEMENT: Splitting a page behaves consistently for immutable, mutable-storage, and vector-backed data.
    ///
    /// HOW:
    /// - Create `123` pages from each storage type, split one byte, and verify the returned `1` and retained `23`.
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

    /// STATEMENT: Reading a page consumes its bytes and leaves unused destination bytes untouched.
    ///
    /// HOW:
    /// - Read `123` into a ten-byte zeroed array and verify a three-byte read, zero remaining length, and seven trailing zeros.
    #[test]
    fn page_read() {
        use std::io::Read;

        let mut page = BytePage::from(Bytes::copy_from_slice(b"123"));

        let mut buf = [0; 10];
        assert_eq!(page.read(&mut buf).unwrap(), 3);
        assert_eq!(page.len(), 0);
        assert_eq!(buf, [49, 50, 51, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// STATEMENT: Page-size updates, prepending, stream writes, and borrowed string appends preserve data.
    ///
    /// HOW:
    /// - Change `Size4` to `Size8`, reject an empty prepend, prepend `a`, write and flush `bc`, and freeze to `abc`.
    /// - Append a borrowed `ByteString` afterward and verify the next frozen value is `str`.
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

    /// STATEMENT: Current-page transfer only fills an empty destination and leaves transferred storage writable.
    ///
    /// HOW:
    /// - Attempt to transfer `data` into a destination containing `x` and check both lengths stay unchanged.
    /// - Transfer into an empty buffer, check source and destination lengths, append `!`, and freeze to `data!`.
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

    /// STATEMENT: The page-list cache enforces its maximum size and skips caching while already in use.
    ///
    /// HOW:
    /// - Drop `CACHE_SIZE + 1` buffers into an empty cache and verify exactly `CACHE_SIZE` lists are retained.
    /// - Take the cache while dropping another buffer, restore it, and verify no list was added.
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

    /// STATEMENT: Page conversions preserve payloads from mutable storage, vectors, and borrowed byte strings.
    ///
    /// HOW:
    /// - Check `Storage` and `Vec` page kinds, convert their 64-byte payloads to `Bytes` or `BytesMut`, and compare contents.
    /// - Convert a borrowed `ByteString` to a page and check `string` remains intact.
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

    /// STATEMENT: Appending unique storage with spare capacity makes it writable current storage, unlike full or shared pages.
    ///
    /// HOW:
    /// - Append a mutable page containing `a`, write `b`, and check a single page freezes to `ab`.
    /// - Append full and shared pages followed by `z`, checking page count, total length, and unchanged shared contents.
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

    // inline page, shared page, then the current page
    fn mixed_pages() -> BytePages {
        let mut pages = BytePages::new(BytePageSize::Size4);
        pages.extend_from_slice(b"current");
        pages.prepend(Bytes::copy_from_slice(&[b'b'; 64]));
        pages.prepend(Bytes::copy_from_slice(b"ab"));
        assert!(pages.pages()[0].is_inline());
        pages
    }

    /// STATEMENT: Page iteration preserves order and addresses across moves and omits empty current pages.
    ///
    /// HOW:
    /// - Check empty iteration, then collect inline, shared, and current chunks from `mixed_pages` and compare their order and count.
    /// - Move the buffer into a box and compare chunk addresses, then consume another buffer and verify empty iteration.
    #[test]
    fn iter_pages() {
        let pages = BytePages::default();
        assert_eq!(pages.iter().count(), 0);

        let pages = mixed_pages();
        let chunks: Vec<&[u8]> = pages.iter().collect();
        assert_eq!(chunks, [&b"ab"[..], &[b'b'; 64][..], &b"current"[..]]);
        assert_eq!(chunks.len(), pages.num_pages());

        // data, including inline pages, does not move with `self`
        let ptrs: Vec<*const u8> = pages.iter().map(<[u8]>::as_ptr).collect();
        let moved = Box::new(pages);
        let moved_ptrs: Vec<*const u8> = moved.iter().map(<[u8]>::as_ptr).collect();
        assert_eq!(ptrs, moved_ptrs);

        // an empty current page is skipped
        let mut pages = BytePages::new(BytePageSize::Size4);
        pages.extend_from_slice(b"data");
        pages.prepend(Bytes::copy_from_slice(b"ab"));
        pages.advance_to(6);
        assert_eq!(pages.iter().count(), 0);
        assert_eq!(pages.num_pages(), 0);
    }

    /// STATEMENT: Advancing across pages drops consumed data while retaining the current page's reusable spare capacity.
    ///
    /// HOW:
    /// - Advance by zero, whole pages, and partial pages in `mixed_pages`, checking lengths, page counts, and remaining chunks.
    /// - Consume the current page, write `next`, and verify its pointer continues after the consumed bytes.
    #[test]
    fn advance_to_pages() {
        let mut pages = mixed_pages();
        let len = pages.len();
        pages.advance_to(0);
        assert_eq!(pages.len(), len);

        // whole page
        pages.advance_to(2);
        assert_eq!(pages.len(), len - 2);
        assert_eq!(pages.num_pages(), 2);
        assert_eq!(pages.iter().next().unwrap(), &[b'b'; 64][..]);

        // part of a page
        pages.advance_to(60);
        assert_eq!(pages.len(), 11);
        assert_eq!(pages.num_pages(), 2);
        let chunks: Vec<&[u8]> = pages.iter().collect();
        assert_eq!(chunks, [&b"bbbb"[..], &b"current"[..]]);

        // rest of the page and part of the current page
        pages.advance_to(6);
        assert_eq!(pages.len(), 5);
        assert_eq!(pages.num_pages(), 1);
        assert_eq!(pages.iter().next().unwrap(), b"rrent");

        // the current page keeps its spare capacity
        let ptr = pages.iter().next().unwrap().as_ptr();
        pages.advance_to(5);
        assert!(pages.is_empty());
        assert_eq!(pages.num_pages(), 0);
        pages.extend_from_slice(b"next");
        assert_eq!(pages.num_pages(), 1);
        assert_eq!(pages.iter().next().unwrap().as_ptr(), ptr.wrapping_add(5));
        assert_eq!(pages.freeze(), "next");
    }

    /// STATEMENT: Moving buffers retains page allocations, destination size settings, and page order without merging small pages.
    ///
    /// HOW:
    /// - Move `mixed_pages` into an empty `Size16` buffer and check source emptiness, destination size, lengths, and non-inline pointers.
    /// - Move into a populated buffer, compare all chunk boundaries, append again, and verify moving an empty source changes nothing.
    #[test]
    fn move_to_keeps_pages() {
        // into empty pages, the page list is swapped
        let mut src = mixed_pages();
        let ptrs: Vec<*const u8> = src
            .iter()
            .filter(|s| s.len() > 2)
            .map(<[u8]>::as_ptr)
            .collect();
        let mut dst = BytePages::new(BytePageSize::Size16);
        src.move_to(&mut dst);
        assert!(src.is_empty());
        assert_eq!(src.num_pages(), 0);
        assert_eq!(dst.page_size(), BytePageSize::Size16);
        assert_eq!(dst.num_pages(), 3);
        assert_eq!(dst.len(), 73);
        let moved: Vec<*const u8> = dst
            .iter()
            .filter(|s| s.len() > 2)
            .map(<[u8]>::as_ptr)
            .collect();
        assert_eq!(ptrs, moved);

        // small pages are not merged, the filled part of the current page
        // of `dst` goes first and its spare capacity is kept
        let mut dst = BytePages::new(BytePageSize::Size4);
        dst.extend_from_slice(b"first");
        dst.prepend(Bytes::copy_from_slice(b"0"));
        let mut src = mixed_pages();
        src.move_to(&mut dst);
        let chunks: Vec<&[u8]> = dst.iter().collect();
        assert_eq!(
            chunks,
            [
                &b"0"[..],
                &b"first"[..],
                &b"ab"[..],
                &[b'b'; 64][..],
                &b"current"[..]
            ]
        );
        assert_eq!(dst.len(), 79);
        dst.extend_from_slice(b"last");
        assert_eq!(dst.num_pages(), 6);

        // empty source
        let mut src = BytePages::default();
        src.move_to(&mut dst);
        assert_eq!(dst.num_pages(), 6);
    }

    /// STATEMENT: Advancing a vector-backed page retains the correct suffix.
    ///
    /// HOW:
    /// - Prepend a vector containing `hello world`, advance six bytes, and verify length five and frozen `world`.
    #[test]
    fn advance_to_vec_page() {
        let mut pages = BytePages::default();
        pages.prepend(b"hello world".to_vec());
        pages.advance_to(6);
        assert_eq!(pages.len(), 5);
        assert_eq!(pages.freeze(), "world");
    }

    /// STATEMENT: Advancing beyond total page-buffer length panics.
    ///
    /// HOW:
    /// - Build `mixed_pages`, advance by its length plus one, and expect the end-of-buffer bounds panic.
    #[test]
    #[should_panic(expected = "cannot advance past the end of the buffer")]
    fn advance_to_past_end() {
        let mut pages = mixed_pages();
        pages.advance_to(pages.len() + 1);
    }
}
