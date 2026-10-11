//! Provided buffer ring for receives that wait for input.
//!
//! A receive that owns its buffer pins a read page for every idle connection
//! until the peer sends something. Receives that select a buffer from the
//! ring take a page only when data arrives, the pages are shared by all
//! connections of the reactor.
//!
//! Ring buffers are regular read pages. A completed receive hands the page
//! over to the connection as its read buffer, without copying, and a fresh
//! page from the page cache takes its slot in the ring.
use std::alloc::{self, Layout};
use std::sync::atomic::{AtomicU16, Ordering};
use std::{io, mem, ptr::NonNull};

use ntex_bytes::{BufMut, BytePageSize, BytesMut};
use ntex_io_uring::types::BufRingEntry;

use super::reactor::ReactorApi;

/// Number of ring buffers
const ENTRIES: u16 = 512;
/// Page size of ring buffers
const PAGE_SIZE: BytePageSize = BytePageSize::Size4;

pub(super) struct BufRing {
    ring: NonNull<BufRingEntry>,
    layout: Layout,
    /// Buffers by buffer id, the kernel writes into buffers that are
    /// published in the ring
    bufs: Box<[BytesMut]>,
    size: BytePageSize,
    tail: u16,
    mask: u16,
    bgid: u16,
    registered: bool,
}

impl BufRing {
    /// Registers a ring of read pages, returns `None` if provided buffer
    /// rings are not supported.
    pub(super) fn new(api: &ReactorApi) -> Option<Self> {
        Self::with_size(api, ENTRIES, PAGE_SIZE)
    }

    fn with_size(api: &ReactorApi, entries: u16, size: BytePageSize) -> Option<Self> {
        debug_assert!(entries.is_power_of_two());

        // the ring must be page aligned
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .ok()
            .filter(|p| p.is_power_of_two())
            .unwrap_or(4096);
        let layout =
            Layout::from_size_align(usize::from(entries) * mem::size_of::<BufRingEntry>(), page)
                .ok()?;
        // SAFETY: the layout has a non-zero size
        let ring = NonNull::new(unsafe { alloc::alloc_zeroed(layout) })?.cast();

        let mut this = BufRing {
            ring,
            layout,
            size,
            bufs: Box::default(),
            tail: 0,
            mask: entries - 1,
            bgid: api.id(),
            registered: false,
        };

        // SAFETY: the ring memory is freed only after it is unregistered
        if let Err(e) = unsafe { api.register_buf_ring(ring.as_ptr() as u64, entries, this.bgid) } {
            log::debug!("Provided buffer rings are not supported: {e:?}");
            return None;
        }
        this.registered = true;

        this.bufs = (0..entries)
            .map(|_| BytesMut::with_page_size(size))
            .collect();
        for bid in 0..entries {
            this.push(bid);
        }
        this.publish();
        Some(this)
    }

    /// Buffer group id of the ring.
    pub(super) fn bgid(&self) -> u16 {
        self.bgid
    }

    /// Size of every ring buffer.
    pub(super) fn buf_len(&self) -> u32 {
        self.bufs.first().map_or(0, |b| b.remaining_mut() as u32)
    }

    /// Takes buffer `bid` with `len` bytes received into it, a fresh page
    /// takes its place in the ring.
    pub(super) fn take(&mut self, bid: u16, len: usize) -> BytesMut {
        let fresh = BytesMut::with_page_size(self.size);
        let mut buf = mem::replace(&mut self.bufs[usize::from(bid)], fresh);
        // SAFETY: the kernel received `len` bytes into the buffer
        unsafe { buf.advance_mut(len) };
        self.push(bid);
        self.publish();
        buf
    }

    /// Returns unused buffer `bid` to the ring.
    pub(super) fn recycle(&mut self, bid: u16) {
        self.push(bid);
        self.publish();
    }

    /// Unregisters the ring, the kernel must not use it afterwards.
    pub(super) fn unregister(&mut self, api: &ReactorApi) -> io::Result<()> {
        if self.registered {
            api.unregister_buf_ring(self.bgid)?;
            self.registered = false;
        }
        Ok(())
    }

    /// Adds buffer `bid` to the ring, it becomes visible to the kernel once
    /// the tail is published.
    fn push(&mut self, bid: u16) {
        let buf = &mut self.bufs[usize::from(bid)];
        let chunk = buf.chunk_mut();
        let (addr, len) = (chunk.as_mut_ptr() as u64, chunk.len() as u32);

        // SAFETY: the index is masked to the ring size, the setters leave
        // the tail that overlays the first entry untouched
        let entry = unsafe { &mut *self.ring.as_ptr().add(usize::from(self.tail & self.mask)) };
        entry.set_addr(addr);
        entry.set_len(len);
        entry.set_bid(bid);
        self.tail = self.tail.wrapping_add(1);
    }

    /// Publishes pushed buffers to the kernel.
    fn publish(&self) {
        // SAFETY: the tail is a properly aligned `u16` within the ring, the
        // kernel reads it with acquire ordering
        unsafe {
            let tail = BufRingEntry::tail(self.ring.as_ptr()).cast::<AtomicU16>();
            (*tail).store(self.tail, Ordering::Release);
        }
    }
}

impl Drop for BufRing {
    fn drop(&mut self) {
        if self.registered {
            // the kernel may still use the ring, leak it
            log::error!(
                "Provided buffer ring {} is dropped while registered",
                self.bgid
            );
            mem::forget(mem::take(&mut self.bufs));
        } else {
            // SAFETY: allocated with the same layout in `with_size`
            unsafe { alloc::dealloc(self.ring.as_ptr().cast(), self.layout) };
        }
    }
}

impl std::fmt::Debug for BufRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufRing")
            .field("bgid", &self.bgid)
            .field("entries", &self.bufs.len())
            .field("size", &self.size)
            .finish()
    }
}
