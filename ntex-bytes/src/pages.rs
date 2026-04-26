use std::{cmp, collections::VecDeque, fmt, mem, ptr};

use crate::{BufMut, Bytes, PageSize, buf::UninitSlice, storage::StorageVec};

pub struct BytesPages {
    size: PageSize,
    pages: VecDeque<Page>,
    current: StorageVec,
}

impl BytesPages {
    /// Creates a new `BytesPages` with the specified page size.
    ///
    /// The returned `BytesPages` will be hold one page with
    /// specified capacity.
    pub fn new(size: PageSize) -> Self {
        debug_assert!(size != PageSize::Unset, "Page cannot be Unset");

        BytesPages {
            size,
            pages: VecDeque::with_capacity(8),
            current: StorageVec::sized(size),
        }
    }

    #[inline]
    pub fn append(&mut self, buf: Bytes) {
        let remaining = self.current.remaining();

        if buf.len() <= remaining {
            self.put_slice(buf.as_ref());
        } else {
            if self.current.len() != 0 {
                // push current storage to stack
                self.pages.push_back(Page::from(mem::replace(
                    &mut self.current,
                    StorageVec::sized(self.size),
                )));
            }

            // add buffer to stack
            self.pages.push_back(Page::from(buf));
        }
    }

    #[inline]
    pub fn append_page(&mut self, page: Page) {
        if self.current.len() != 0 {
            // push current page to stack
            self.pages.push_back(Page::from(mem::replace(
                &mut self.current,
                StorageVec::sized(self.size),
            )));
        }

        // add page to stack
        self.pages.push_back(page);
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
    pub fn len(&self) -> usize {
        self.pages
            .iter()
            .fold(self.current.len(), |c, page| c + page.len())
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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

    #[inline]
    pub fn take(&mut self) -> Option<Page> {
        if let Some(page) = self.pages.pop_front() {
            Some(page)
        } else if self.current.len() == 0 {
            None
        } else {
            Some(Page::from(mem::replace(
                &mut self.current,
                StorageVec::sized(self.size),
            )))
        }
    }
}

impl fmt::Debug for BytesPages {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = fmt.debug_tuple("Pages");
        for p in &self.pages {
            f.field(p);
        }
        if self.current.len() != 0 {
            f.field(&crate::debug::BsDebug(self.current.as_ref()));
        }
        f.finish()
    }
}

impl BufMut for BytesPages {
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
                self.pages.push_back(Page::from(mem::replace(
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
            self.pages.push_back(Page::from(mem::replace(
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

pub struct Page {
    inner: StorageType,
}

enum StorageType {
    Bytes(Bytes),
    Storage(StorageVec),
}

impl Page {
    #[inline]
    pub fn len(&self) -> usize {
        match &self.inner {
            StorageType::Bytes(b) => b.len(),
            StorageType::Storage(b) => b.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        match &self.inner {
            StorageType::Bytes(b) => b.is_empty(),
            StorageType::Storage(b) => b.len() == 0,
        }
    }

    /// Return a raw pointer to data
    pub fn as_ptr(&self) -> *const u8 {
        unsafe {
            match &self.inner {
                StorageType::Bytes(b) => b.storage.as_ptr(),
                StorageType::Storage(b) => b.as_ptr(),
            }
        }
    }
}

impl AsRef<[u8]> for Page {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        match &self.inner {
            StorageType::Bytes(b) => b.as_ref(),
            StorageType::Storage(b) => b.as_ref(),
        }
    }
}

impl From<Bytes> for Page {
    fn from(buf: Bytes) -> Self {
        Page {
            inner: StorageType::Bytes(buf),
        }
    }
}

impl From<StorageVec> for Page {
    fn from(buf: StorageVec) -> Self {
        Page {
            inner: StorageType::Storage(buf),
        }
    }
}

impl fmt::Debug for Page {
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
        let mut pages = BytesPages::new(PageSize::Size8);
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

        let mut pgs = BytesPages::new(PageSize::Size8);
        pgs.put_i8(b'a' as i8);
        let p = pgs.take().unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p.as_ref(), b"a");

        pgs.extend_from_slice("a".repeat(8 * 1024 - 1).as_bytes());
        assert_eq!(pgs.num_pages(), 1);
        pgs.put_u8(b'a');
        assert_eq!(pgs.num_pages(), 1);
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

        let p = Page::from(Bytes::copy_from_slice(b"123"));
        assert_eq!(p.len(), 3);
        assert!(!p.is_empty());
        assert_eq!(p.as_ref(), b"123");
        assert_eq!(p.as_ref().as_ptr(), p.as_ptr());

        // debug
        let mut pages = BytesPages::new(PageSize::Size8);
        pages.extend_from_slice(b"b");
        let p = pages.take().unwrap();
        assert_eq!(p.as_ref(), b"b");

        let mut pages = BytesPages::new(PageSize::Size8);
        pages.extend_from_slice(b"a");
        pages.append(Bytes::copy_from_slice(b"123"));
        pages.append_page(p);
        assert_eq!(format!("{pages:?}"), "Pages(b\"a123\", b\"b\")");
    }
}
