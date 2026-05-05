use std::{borrow::Borrow, cmp, collections::VecDeque, fmt, io, mem, ops, ptr};

use crate::{BufMut, BytePageSize, ByteString, Bytes, BytesMut};
use crate::{buf::UninitSlice, storage::StorageVec};

pub struct BytePages {
    size: BytePageSize,
    pages: VecDeque<BytePage>,
    current: StorageVec,
}

impl BytePages {
    /// Creates a new `BytePages` with the specified page size.
    ///
    /// The returned `BytePages` will be hold one page with
    /// specified capacity.
    pub fn new(size: BytePageSize) -> Self {
        debug_assert!(size != BytePageSize::Unset, "Page cannot be Unset");

        BytePages {
            size,
            pages: VecDeque::with_capacity(8),
            current: StorageVec::sized(size),
        }
    }

    /// Get size of the page.
    pub fn page_size(&self) -> BytePageSize {
        self.size
    }

    /// Sets the page size for new pages.
    pub fn set_page_size(&mut self, size: BytePageSize) {
        self.size = size;
    }

    /// Insert a page to the front of the collection.
    pub fn prepend<T>(&mut self, buf: T) -> bool
    where
        BytePage: From<T>,
    {
        let p = BytePage::from(buf);
        if p.is_empty() {
            false
        } else {
            self.pages.push_front(p);
            true
        }
    }

    /// Appends a new page to the back of the collection.
    pub fn append<T>(&mut self, buf: T)
    where
        BytePage: From<T>,
    {
        let p = BytePage::from(buf);
        let remaining = self.current.remaining();

        if p.len() <= remaining {
            self.put_slice(p.as_ref());
        } else if self.current.len() == 0 {
            match p.into_storage() {
                Ok(st) => {
                    self.current = st;
                }
                Err(page) => {
                    // add buffer to stack
                    self.pages.push_back(page);
                }
            }
        } else {
            // push current storage to stack
            self.pages.push_back(BytePage {
                inner: StorageType::Storage(mem::replace(
                    &mut self.current,
                    StorageVec::sized(self.size),
                )),
            });

            // add buffer to stack
            self.pages.push_back(p);
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
    /// Gets the total number of pages.
    pub fn len(&self) -> usize {
        self.pages
            .iter()
            .fold(self.current.len(), |c, page| c + page.len())
    }

    #[inline]
    /// Checks if the `BytePages` instance is empty.
    pub fn is_empty(&self) -> bool {
        for p in &self.pages {
            if !p.is_empty() {
                return false;
            }
        }
        self.current.len() == 0
    }

    #[inline]
    /// Returns the total number of pages contained in this object.
    pub fn num_pages(&self) -> usize {
        if self.current.len() == 0 {
            self.pages.len()
        } else {
            self.pages.len() + 1
        }
    }

    /// Returns the first page from the collection.
    pub fn take(&mut self) -> Option<BytePage> {
        if let Some(page) = self.pages.pop_front() {
            Some(page)
        } else if self.current.len() == 0 {
            None
        } else {
            Some(BytePage::from(mem::replace(
                &mut self.current,
                StorageVec::sized(self.size),
            )))
        }
    }

    #[inline]
    /// Copies all pages into another `BytePages` instance.
    ///
    /// Depending on the underlying storage, this operation might be `O(1)` or could
    /// involve a memory copy.
    pub fn copy_to(&self, pages: &mut BytePages) {
        for p in &self.pages {
            pages.append(p.clone());
        }

        if self.current.len() != 0 {
            pages.append(BytePage::from(Bytes::copy_from_slice(
                self.current.as_ref(),
            )));
        }
    }

    #[inline]
    /// Moves all pages to another `BytePages` instance.
    pub fn move_to(&mut self, pages: &mut BytePages) {
        while let Some(page) = self.take() {
            pages.append(page);
        }
    }

    /// Splits the buffer into two at the given index.
    ///
    /// Afterwards, `self` contains elements `[at, len)`, and the returned `BytePage`
    /// contains elements `[0, at)`.
    ///
    /// Depending on the underlying storage, this operation might be `O(1)` or could
    /// involve a memory copy.
    #[must_use]
    pub fn split_to(&mut self, at: usize) -> BytePages {
        let mut pages = BytePages::new(self.size);
        self.split_into(at, &mut pages);
        pages
    }

    /// Splits the buffer, adding the resulting items to the supplied pages object.
    ///
    /// Afterwards, `self` contains elements `[at, len)`, and the supplied `BytePage`
    /// contains elements `[0, at)`.
    ///
    /// Depending on the underlying storage, this operation might be `O(1)` or could
    /// involve a memory copy.
    pub fn split_into(&mut self, mut at: usize, to: &mut BytePages) {
        while let Some(mut page) = self.pages.pop_front() {
            let len = cmp::min(page.len(), at);
            to.append(page.split_to(len));

            if !page.is_empty() {
                self.pages.push_front(page);
                return;
            }
            at -= len;
        }

        if at > 0
            && let Some(mut page) = self.take()
        {
            let len = cmp::min(page.len(), at);
            to.append(page.split_to(len));
            self.append(page);
        }
    }

    /// Converts `self` into an immutable `Bytes`.
    #[inline]
    #[must_use]
    pub fn freeze(&mut self) -> Bytes {
        let mut buf = BytesMut::with_capacity(self.len());
        while let Some(p) = self.take() {
            buf.extend_from_slice(&p);
        }
        buf.freeze()
    }

    #[inline]
    pub fn try_get_current_from(&mut self, pages: &mut BytePages) {
        if self.pages.is_empty() && self.current.len() == 0 && pages.current.len() != 0 {
            self.current = mem::replace(&mut pages.current, StorageVec::sized(self.size));
        }
    }
}

impl fmt::Debug for BytePages {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = fmt.debug_tuple("BytePages");
        for p in &self.pages {
            f.field(p);
        }
        if self.current.len() != 0 {
            f.field(&crate::debug::BsDebug(self.current.as_ref()));
        }
        f.finish()
    }
}

impl Default for BytePages {
    fn default() -> Self {
        BytePages::new(BytePageSize::Size16)
    }
}

impl BufMut for BytePages {
    #[inline]
    fn remaining_mut(&self) -> usize {
        self.current.remaining()
    }

    #[inline]
    unsafe fn advance_mut(&mut self, cnt: usize) {
        // This call will panic if `cnt` is too big
        self.current.set_len(self.current.len() + cnt);
    }

    #[inline]
    fn chunk_mut(&mut self) -> &mut UninitSlice {
        unsafe {
            // This will never panic as `len` can never become invalid
            let ptr = &mut self.current.as_ptr();
            UninitSlice::from_raw_parts_mut(
                ptr.add(self.current.len()),
                self.remaining_mut(),
            )
        }
    }

    fn put_slice(&mut self, mut src: &[u8]) {
        while !src.is_empty() {
            let amount = cmp::min(src.len(), self.current.remaining());
            unsafe {
                ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    self.chunk_mut().as_mut_ptr(),
                    amount,
                );
                self.advance_mut(amount);
            }
            src = &src[amount..];

            // add new page
            if self.current.is_full() {
                self.pages.push_back(BytePage::from(mem::replace(
                    &mut self.current,
                    StorageVec::sized(self.size),
                )));
            }
        }
    }

    #[inline]
    fn put_u8(&mut self, n: u8) {
        self.current.put_u8(n);
        if self.current.is_full() {
            self.pages.push_back(BytePage::from(mem::replace(
                &mut self.current,
                StorageVec::sized(self.size),
            )));
        }
    }

    #[inline]
    fn put_i8(&mut self, n: i8) {
        self.put_u8(n as u8);
    }
}

impl Clone for BytePages {
    fn clone(&self) -> Self {
        let mut pages = BytePages::new(self.size);
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
    fn from(pages: BytePages) -> Bytes {
        BytesMut::from(pages).freeze()
    }
}

impl From<BytePages> for BytesMut {
    fn from(mut pages: BytePages) -> BytesMut {
        let mut buf = BytesMut::with_capacity(pages.len());
        while let Some(p) = pages.take() {
            buf.extend_from_slice(&p);
        }
        buf
    }
}

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
        match &self.inner {
            StorageType::Bytes(b) => b.len(),
            StorageType::Storage(b) => b.len(),
            StorageType::Vec(b) => b.len(),
        }
    }

    #[inline]
    /// Returns true if the `BytePage` has a length of 0.
    pub fn is_empty(&self) -> bool {
        match &self.inner {
            StorageType::Bytes(b) => b.is_empty(),
            StorageType::Storage(b) => b.len() == 0,
            StorageType::Vec(b) => b.is_empty(),
        }
    }

    /// Return a raw pointer to data.
    pub fn as_ptr(&self) -> *const u8 {
        unsafe {
            match &self.inner {
                StorageType::Bytes(b) => b.storage.as_ptr(),
                StorageType::Storage(b) => b.as_ptr(),
                StorageType::Vec(b) => b.as_ptr(),
            }
        }
    }

    /// Splits the buffer into two at the given index.
    ///
    /// Afterwards, `self` contains elements `[at, len)`, and the returned `BytePage`
    /// contains elements `[0, at)`.
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
    /// This is an `O(1)` operation.
    ///
    /// # Panics
    ///
    /// Panics if `cnt > len`.
    #[inline]
    pub fn advance_to(&mut self, cnt: usize) {
        match &mut self.inner {
            StorageType::Bytes(b) => b.advance_to(cnt),
            StorageType::Storage(b) => unsafe { b.set_start(cnt as u32) },
            StorageType::Vec(b) => {
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
        if let StorageType::Storage(mut st) = self.inner {
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
            StorageType::Storage(st) => {
                // SAFETY: We garantee that `st` is not being used
                // for modification. `st` is marked as non-unique after clone
                StorageType::Storage(unsafe { st.clone() })
            }
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
        BytePage::from(Bytes::from_static(buf.as_bytes()))
    }
}

impl From<&'static [u8]> for BytePage {
    fn from(buf: &'static [u8]) -> Self {
        BytePage::from(Bytes::from_static(buf))
    }
}

impl<const N: usize> From<&'static [u8; N]> for BytePage {
    fn from(src: &'static [u8; N]) -> Self {
        BytePage::from(Bytes::from_static(src))
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
            StorageType::Storage(storage) => BytesMut { storage },
            StorageType::Vec(v) => BytesMut::copy_from_slice(&v),
        }
    }
}

impl PartialEq for BytePage {
    fn eq(&self, other: &BytePage) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl<'a> PartialEq<&'a [u8]> for BytePage {
    fn eq(&self, other: &&'a [u8]) -> bool {
        self.as_ref() == *other
    }
}

impl<'a, const N: usize> PartialEq<&'a [u8; N]> for BytePage {
    fn eq(&self, other: &&'a [u8; N]) -> bool {
        self.as_ref() == other.as_ref()
    }
}

impl io::Read for BytePage {
    fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
        let len = cmp::min(self.len(), dst.len());
        if len > 0 {
            dst[..len].copy_from_slice(&self[..len]);
            self.advance_to(len);
        }
        Ok(len)
    }
}

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
    use super::*;

    #[test]
    fn pages() {
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

        pgs.extend_from_slice("a".repeat(8 * 1024 - 1).as_bytes());
        assert_eq!(pgs.num_pages(), 1);
        pgs.put_u8(b'a');
        assert_eq!(pgs.num_pages(), 1);
        assert_eq!(pgs.current.len(), 0);

        pgs.put_u8(b'a');
        assert_eq!(pgs.num_pages(), 2);

        pgs.append(Bytes::copy_from_slice("a".repeat(8 * 1024).as_bytes()));
        assert_eq!(pgs.num_pages(), 3);
        assert_eq!(pgs.current.len(), 0);

        // page
        let p = pages.take().unwrap();
        assert_eq!(p.len(), 8192);
        let p = pages.take().unwrap();
        assert_eq!(p.len(), 1025);
        assert!(!p.is_empty());
        assert_eq!(p.as_ref().as_ptr(), p.as_ptr());
        assert_eq!(p.as_ref(), "a".repeat(1025).as_bytes());
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
        pages.pages.push_back(p);
        assert_eq!(format!("{pages:?}"), "BytePages(b\"b\", b\"a123\")");
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
    fn pages_split_to() {
        let mut pages = BytePages::default();
        pages.put_slice(b"456");
        pages.prepend(BytePage::from(Bytes::copy_from_slice(b"123")));
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
        if let StorageType::Storage(mut st) = p.inner {
            assert!(!st.is_unique());
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
}
