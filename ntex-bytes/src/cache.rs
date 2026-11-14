//! Per-thread page cache and its statistics.
use std::cell::RefCell;
use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering::Relaxed};

use crate::stvec::{Header, StorageVec};
use crate::{BytePageSize, size::PAGE_SIZES};

/// Number of page sizes, `BytePageSize::Unset` excluded.
const CLASSES: usize = PAGE_SIZES.len();
const _: () = assert!(CLASSES == BytePageSize::Unset as usize);

/// Default number of cached pages per page size, fewer pages are cached
/// for larger sizes. Reads start at 4 KiB pages and adapt up to 64 KiB
/// pages under load, 16 KiB is the default page size of writes.
const DEFAULT_PAGES_CACHE: [usize; CLASSES] = [128, 64, 64, 32, 16, 8, 16, 2, 1];

thread_local! {
    // inline, not boxed, the hot paths save a dependent load
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

struct Cache {
    limits: [usize; CLASSES],
    pages: [Vec<StorageVec>; CLASSES],
    metrics: Arc<PageCacheMetrics>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            limits: DEFAULT_PAGES_CACHE,
            pages: Default::default(),
            metrics: PageCacheMetrics::new(),
        }
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        for (class, pages) in self.pages.iter().enumerate() {
            self.metrics.remove_pages(class, pages.len());
        }
    }
}

impl Cache {
    /// Sets the limit of `class`, the pages above the limit are released.
    fn set_limit(&mut self, class: usize, limit: usize) {
        self.limits[class] = limit;
        let pages = &mut self.pages[class];
        if pages.len() > limit {
            self.metrics.remove_pages(class, pages.len() - limit);
            // the cache is borrowed, the released pages are freed instead of
            // being cached again
            pages.truncate(limit);
        }
    }
}

/// Runs `f` with the cache of the current thread.
///
/// Returns `None` if the cache is in use or the thread-local is being
/// destroyed.
#[inline]
fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> Option<R> {
    CACHE
        .try_with(|c| c.try_borrow_mut().ok().map(|mut cst| f(&mut cst)))
        .ok()
        .flatten()
}

/// Takes a page of size `size` from the cache.
#[inline]
pub(crate) fn take_page(size: BytePageSize) -> Option<StorageVec> {
    let class = size as usize;
    with_cache(|cst| {
        let page = cst.pages[class].pop();
        if page.is_some() {
            cst.metrics.hit(class);
        } else {
            cst.metrics.miss(class);
        }
        page
    })
    .flatten()
}

/// Takes a page for a new buffer of page size `size` from the cache.
///
/// If no page of `size` is cached, a cached page of the next smaller size
/// is taken instead. Growing buffers use [`take_page`], they need the exact
/// size.
#[inline]
pub(crate) fn take_page_or_prev(size: BytePageSize) -> Option<StorageVec> {
    let class = size as usize;
    with_cache(|cst| {
        if let Some(page) = cst.pages[class].pop() {
            cst.metrics.hit(class);
            return Some(page);
        }
        cst.metrics.miss(class);

        let prev = size.prev() as usize;
        if prev != class
            && let Some(page) = cst.pages[prev].pop()
        {
            cst.metrics.hit(prev);
            cst.metrics.fallback(class);
            return Some(page);
        }
        None
    })
    .flatten()
}

/// Puts a released page of size `size` to the cache.
///
/// Returns `false` if the page is not cached and must be freed.
///
/// # Safety
///
/// `ptr` must point to a live `Header` of page size `size` without
/// references.
#[inline]
pub(crate) unsafe fn put_page(ptr: *mut Header, size: BytePageSize) -> bool {
    let class = size as usize;
    with_cache(|cst| {
        if cst.pages[class].len() < cst.limits[class] {
            // SAFETY: The caller transfers a live page with no references; the cache takes
            // exclusive ownership.
            cst.pages[class].push(unsafe { StorageVec::recycle(ptr) });
            cst.metrics.cached(class);
            true
        } else {
            cst.metrics.freed(class);
            false
        }
    })
    .unwrap_or(false)
}

/// Sets the cache limit of every page size, see [`crate::set_pages_cache`].
pub(crate) fn set_pages_cache(count: usize) {
    with_cache(|cst| {
        for class in 0..CLASSES {
            cst.set_limit(class, count);
        }
    });
}

/// Sets the cache limit of page size `size`, see [`crate::set_page_cache_size`].
pub(crate) fn set_page_cache_size(size: BytePageSize, count: usize) {
    if size != BytePageSize::Unset {
        with_cache(|cst| cst.set_limit(size as usize, count));
    }
}

#[cfg(test)]
/// Replaces the cache of the current thread with an empty one.
///
/// The old cache is dropped while the cache is borrowed, so its pages are
/// freed rather than returned to the new cache.
pub(crate) fn reset() {
    CACHE.with(|c| {
        let mut cst = c.borrow_mut();
        drop(std::mem::take(&mut *cst));
    });
}

#[cfg(test)]
/// Returns the number of cached pages of size `size`.
pub(crate) fn cached_pages(size: BytePageSize) -> usize {
    with_cache(|cst| cst.pages[size as usize].len()).unwrap()
}

/// Page cache statistics.
///
/// Each thread has its own page cache, see [`crate::set_page_cache_size`].
/// The cache reports to the metrics that are current on its thread, see
/// [`PageCacheMetrics::current`]. Statistics are updated with relaxed atomics,
/// a snapshot is not a consistent cut across all fields.
///
/// Updates are a plain load and store, not an atomic read-modify-write, so
/// metrics must be updated by one thread at a time. If the same metrics are
/// installed on several threads with [`PageCacheMetrics::set_current`],
/// concurrent updates can be lost, counters undercount and the `pages` and
/// `bytes` gauges drift permanently.
///
/// A page released on another thread goes to the cache of that thread, so
/// allocations and releases of the same page can be reported to different
/// metrics.
#[derive(Debug, Default)]
pub struct PageCacheMetrics {
    classes: [Class; CLASSES],
}

#[derive(Debug, Default)]
struct Class {
    hits: AtomicU64,
    misses: AtomicU64,
    fallbacks: AtomicU64,
    cached: AtomicU64,
    freed: AtomicU64,
    // pages that entered or left the cache other than by `cached` and `hits`,
    // the pages gauge is derived, so the hot paths update one counter each
    added: AtomicU64,
    removed: AtomicU64,
}

/// A snapshot of [`PageCacheMetrics`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PageCacheSnapshot {
    sizes: [PageCacheStats; CLASSES],
}

/// Page cache statistics of one page size.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageCacheStats {
    /// Page size, [`BytePageSize::Unset`] for the totals of all page sizes.
    pub size: BytePageSize,
    /// Number of pages of this size taken from the cache, including pages
    /// taken in place of the next larger size, see `fallbacks`.
    pub hits: u64,
    /// Number of requests for this page size that found no cached page of
    /// this size.
    ///
    /// `misses - fallbacks` pages were allocated.
    pub misses: u64,
    /// Number of misses of a new buffer served with a cached page of the
    /// next smaller size, which is counted as a hit of that size.
    pub fallbacks: u64,
    /// Number of released pages returned to the cache.
    pub cached: u64,
    /// Number of released pages freed because the cache was full.
    pub freed: u64,
    /// Number of pages in the cache.
    pub pages: usize,
    /// Memory held by the pages in the cache, in bytes.
    pub bytes: usize,
}

impl PageCacheMetrics {
    /// Creates new metrics.
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Returns the metrics of the current thread's page cache.
    ///
    /// Each thread has its own metrics, created with the cache, unless other
    /// metrics were installed with [`PageCacheMetrics::set_current`].
    pub fn current() -> Arc<Self> {
        with_cache(|cst| cst.metrics.clone()).unwrap_or_default()
    }

    /// Installs `metrics` as the metrics of the current thread's page cache.
    ///
    /// The pages held by the cache are moved from the previous metrics to
    /// `metrics`. Metrics must not be current on more than one thread at a
    /// time, updates are not atomic read-modify-writes.
    pub fn set_current(metrics: Arc<Self>) {
        with_cache(|cst| {
            for (class, pages) in cst.pages.iter().enumerate() {
                cst.metrics.remove_pages(class, pages.len());
                metrics.add_pages(class, pages.len());
            }
            cst.metrics = metrics;
        });
    }

    /// Returns a snapshot of the statistics.
    pub fn snapshot(&self) -> PageCacheSnapshot {
        let mut snapshot = PageCacheSnapshot::default();
        for (stats, class) in snapshot.sizes.iter_mut().zip(&self.classes) {
            stats.hits = class.hits.load(Relaxed);
            stats.misses = class.misses.load(Relaxed);
            stats.fallbacks = class.fallbacks.load(Relaxed);
            stats.cached = class.cached.load(Relaxed);
            stats.freed = class.freed.load(Relaxed);
            let removed = class.removed.load(Relaxed);
            let hits = stats.hits;
            // fields are read one by one, a racing update can make it negative
            stats.pages =
                (stats.cached + class.added.load(Relaxed)).saturating_sub(hits + removed) as usize;
            stats.bytes = stats.pages * stats.size.alloc_size();
        }
        snapshot
    }

    pub(crate) fn hit(&self, class: usize) {
        inc(&self.classes[class].hits, 1);
    }

    pub(crate) fn miss(&self, class: usize) {
        inc(&self.classes[class].misses, 1);
    }

    pub(crate) fn fallback(&self, class: usize) {
        inc(&self.classes[class].fallbacks, 1);
    }

    pub(crate) fn cached(&self, class: usize) {
        inc(&self.classes[class].cached, 1);
    }

    pub(crate) fn freed(&self, class: usize) {
        inc(&self.classes[class].freed, 1);
    }

    /// Pages leave the cache without being taken, e.g. the cache is dropped.
    pub(crate) fn remove_pages(&self, class: usize, pages: usize) {
        if pages != 0 {
            inc(&self.classes[class].removed, pages as u64);
        }
    }

    pub(crate) fn add_pages(&self, class: usize, pages: usize) {
        if pages != 0 {
            inc(&self.classes[class].added, pages as u64);
        }
    }
}

// single writer, cheaper than `fetch_add`
#[inline]
fn inc(counter: &AtomicU64, n: u64) {
    counter.store(counter.load(Relaxed).wrapping_add(n), Relaxed);
}

impl PageCacheSnapshot {
    /// Returns the statistics of page size `size`, `None` for
    /// [`BytePageSize::Unset`].
    pub fn get(&self, size: BytePageSize) -> Option<&PageCacheStats> {
        self.sizes.get(size as usize)
    }

    /// Returns the statistics of every page size, in increasing order of size.
    pub fn iter(&self) -> impl Iterator<Item = &PageCacheStats> {
        self.sizes.iter()
    }

    /// Returns the totals of all page sizes.
    pub fn total(&self) -> PageCacheStats {
        self.sizes.iter().fold(
            PageCacheStats {
                size: BytePageSize::Unset,
                ..PageCacheStats::default()
            },
            |mut total, s| {
                total.hits += s.hits;
                total.misses += s.misses;
                total.fallbacks += s.fallbacks;
                total.cached += s.cached;
                total.freed += s.freed;
                total.pages += s.pages;
                total.bytes += s.bytes;
                total
            },
        )
    }
}

impl Default for PageCacheSnapshot {
    fn default() -> Self {
        Self {
            sizes: PAGE_SIZES.map(|size| PageCacheStats {
                size,
                ..PageCacheStats::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    /// STATEMENT: Dropped page storage is reused by the next allocation in the same size class.
    ///
    /// HOW:
    /// - Reset the cache, write to a `Size8` page, drop it, and check the next `Size8` allocation has its address.
    #[test]
    fn cached() {
        reset();

        let mut st = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(st.page_size(), BytePageSize::Size8);

        st.put_u8(b'h');
        let addr = st.0;
        drop(st);

        let st = StorageVec::sized(BytePageSize::Size8);
        assert_eq!(addr, st.0);
    }

    /// STATEMENT: Cache metrics distinguish hits, misses, cached pages, and full-cache frees by size class.
    ///
    /// HOW:
    /// - Allocate, cache, reuse, and free a `Size8` page and check counters and byte totals.
    /// - Cache `Size4` and non-page buffers, verify totals exclude non-pages, then reset and check old resident counts.
    #[test]
    fn cache_metrics() {
        reset();
        let metrics = PageCacheMetrics::current();
        assert_eq!(metrics.snapshot(), PageCacheSnapshot::default());

        let size = BytePageSize::Size8;
        let st = StorageVec::sized(size);
        let s = *metrics.snapshot().get(size).unwrap();
        assert_eq!(
            (s.size, s.hits, s.misses, s.cached, s.pages),
            (size, 0, 1, 0, 0)
        );

        drop(st);
        let s = *metrics.snapshot().get(size).unwrap();
        assert_eq!(
            (s.hits, s.misses, s.cached, s.freed, s.pages),
            (0, 1, 1, 0, 1)
        );
        assert_eq!(s.bytes, 8 * 1024);

        let st = StorageVec::sized(size);
        let s = *metrics.snapshot().get(size).unwrap();
        assert_eq!(
            (s.hits, s.misses, s.cached, s.pages, s.bytes),
            (1, 1, 1, 0, 0)
        );

        crate::set_page_cache_size(size, 0);
        drop(st);
        let s = *metrics.snapshot().get(size).unwrap();
        assert_eq!((s.cached, s.freed, s.pages), (1, 1, 0));

        // other page sizes are separate, non-page buffers are not counted
        drop(StorageVec::sized(BytePageSize::Size4));
        drop(StorageVec::with_capacity(100));
        let snapshot = metrics.snapshot();
        let s = *snapshot.get(BytePageSize::Size4).unwrap();
        assert_eq!((s.misses, s.cached, s.pages), (1, 1, 1));
        assert!(snapshot.get(BytePageSize::Unset).is_none());
        assert_eq!(snapshot.iter().count(), 9);
        let total = snapshot.total();
        assert_eq!(total.size, BytePageSize::Unset);
        assert_eq!(
            (total.hits, total.misses, total.cached, total.freed),
            (1, 2, 2, 1)
        );
        assert_eq!((total.pages, total.bytes), (1, 4 * 1024));

        // a replaced cache leaves the metrics
        reset();
        assert_eq!(metrics.snapshot().total().pages, 0);
        assert!(!Arc::ptr_eq(&metrics, &PageCacheMetrics::current()));
    }

    /// STATEMENT: Replacing current metrics transfers resident accounting without sharing metrics across threads.
    ///
    /// HOW:
    /// - Cache a `Size16` page, install metrics, and verify resident pages transfer and only new reuse counters change.
    /// - Compare metrics identity with another thread and confirm its metrics are distinct.
    #[test]
    fn cache_metrics_set_current() {
        reset();
        let size = BytePageSize::Size16;
        let old = PageCacheMetrics::current();
        drop(StorageVec::sized(size));
        assert_eq!(old.snapshot().get(size).unwrap().pages, 1);

        let metrics = PageCacheMetrics::new();
        PageCacheMetrics::set_current(metrics.clone());
        assert!(Arc::ptr_eq(&metrics, &PageCacheMetrics::current()));
        assert_eq!(old.snapshot().get(size).unwrap().pages, 0);
        assert_eq!(metrics.snapshot().get(size).unwrap().pages, 1);

        drop(StorageVec::sized(size));
        let s = *metrics.snapshot().get(size).unwrap();
        assert_eq!((s.hits, s.misses, s.cached, s.pages), (1, 0, 1, 1));
        assert_eq!(old.snapshot().get(size).unwrap().hits, 0);

        // other threads have their own metrics
        let other = std::thread::spawn(PageCacheMetrics::current)
            .join()
            .unwrap();
        assert!(!Arc::ptr_eq(&metrics, &other));
        reset();
    }

    /// STATEMENT: Each page size retains no more than its default cache limit.
    ///
    /// HOW:
    /// - Check default limits, allocate four excess pages per size class, drop them all, and verify each cached count equals its limit.
    #[test]
    fn default_cache_limit_per_page_size() {
        assert_eq!(DEFAULT_PAGES_CACHE, [128, 64, 64, 32, 16, 8, 16, 2, 1]);
        for size in crate::size::PAGE_SIZES {
            reset();

            let limit = DEFAULT_PAGES_CACHE[size as usize];
            let pages: Vec<_> = (0..limit + 4).map(|_| StorageVec::sized(size)).collect();
            drop(pages);
            assert_eq!(cached_pages(size), limit, "{size:?}");
        }
        reset();
    }

    /// STATEMENT: Global cache limits control retained pages and ignore reentrant updates.
    ///
    /// HOW:
    /// - Set limits of one and zero, drop pages, and check cached counts.
    /// - Borrow `CACHE`, drop a page and attempt a limit update, then verify no page was cached and limits stayed unchanged.
    #[test]
    #[allow(deprecated)]
    fn pages_cache_size() {
        reset();

        crate::set_pages_cache(1);
        drop((
            StorageVec::sized(BytePageSize::Size4),
            StorageVec::sized(BytePageSize::Size4),
        ));
        assert_eq!(cached_pages(BytePageSize::Size4), 1);

        crate::set_pages_cache(0);
        drop(StorageVec::sized(BytePageSize::Size8));
        assert_eq!(cached_pages(BytePageSize::Size8), 0);

        // the cache is in use, the page is freed
        crate::set_pages_cache(16);
        let st = StorageVec::sized(BytePageSize::Size16);
        CACHE.with(|c| {
            let _cst = c.borrow_mut();
            drop(st);
        });
        assert_eq!(cached_pages(BytePageSize::Size16), 0);

        // the setting is ignored while the cache is in use
        CACHE.with(|c| {
            let _cst = c.borrow_mut();
            crate::set_pages_cache(3);
        });
        assert_eq!(cache_limits(), [16; CLASSES]);
    }

    fn cache_limits() -> [usize; CLASSES] {
        CACHE.with(|c| c.borrow().limits)
    }

    /// STATEMENT: Per-size limits leave other classes unchanged and ignore invalid or reentrant updates.
    ///
    /// HOW:
    /// - Limit `Size64` to one, configure `Unset`, and check limits and retained `Size64` and `Size48` pages.
    /// - Verify an update while `CACHE` is borrowed is ignored, then check a global limit applies to all classes.
    #[test]
    #[allow(deprecated)]
    fn page_cache_size() {
        reset();

        crate::set_page_cache_size(BytePageSize::Size64, 1);
        crate::set_page_cache_size(BytePageSize::Unset, 5);
        let mut expected = DEFAULT_PAGES_CACHE;
        expected[BytePageSize::Size64 as usize] = 1;
        assert_eq!(cache_limits(), expected);

        drop((
            StorageVec::sized(BytePageSize::Size64),
            StorageVec::sized(BytePageSize::Size64),
            StorageVec::sized(BytePageSize::Size48),
            StorageVec::sized(BytePageSize::Size48),
        ));
        assert_eq!(cached_pages(BytePageSize::Size64), 1);
        assert_eq!(cached_pages(BytePageSize::Size48), 2);

        // the setting is ignored while the cache is in use
        CACHE.with(|c| {
            let _cst = c.borrow_mut();
            crate::set_page_cache_size(BytePageSize::Size48, 3);
        });
        assert_eq!(cache_limits(), expected);

        crate::set_pages_cache(2);
        assert_eq!(cache_limits(), [2; CLASSES]);
        reset();
    }

    /// STATEMENT: Lowering cache limits releases excess pages without counting them as full-cache frees.
    ///
    /// HOW:
    /// - Cache four `Size4` and four `Size64` pages, lower per-class and global limits, and check resident counts and counters.
    /// - Raise the remaining class's limit and verify the retained page stays cached.
    #[test]
    #[allow(deprecated)]
    fn lowered_limit_releases_pages() {
        reset();
        let metrics = PageCacheMetrics::current();
        let pages = |size| metrics.snapshot().get(size).unwrap().pages;

        let (s4, s64) = (BytePageSize::Size4, BytePageSize::Size64);
        drop([0; 4].map(|_| StorageVec::sized(s64)));
        drop([0; 4].map(|_| StorageVec::sized(s4)));
        assert_eq!((cached_pages(s64), pages(s64)), (4, 4));

        crate::set_page_cache_size(s64, 1);
        assert_eq!((cached_pages(s64), pages(s64)), (1, 1));
        assert_eq!((cached_pages(s4), pages(s4)), (4, 4));
        crate::set_page_cache_size(s64, 0);
        assert_eq!((cached_pages(s64), pages(s64)), (0, 0));

        // released pages are not counted as freed by a full cache
        crate::set_pages_cache(1);
        assert_eq!((cached_pages(s4), pages(s4)), (1, 1));
        let total = metrics.snapshot().total();
        assert_eq!((total.cached, total.freed, total.pages), (8, 0, 1));

        // a raised limit keeps the cached pages
        crate::set_page_cache_size(s4, 8);
        assert_eq!((cached_pages(s4), pages(s4)), (1, 1));
        reset();
    }

    /// STATEMENT: Page-sized `BytesMut` storage is cached only after its last shared view drops.
    ///
    /// HOW:
    /// - Split a `Size8` buffer, drop both handles in turn, and check cache counts and pointer reuse.
    /// - Verify `Unset` selects `Size64` while ordinary-capacity and copied buffers remain uncached.
    #[test]
    fn bytes_mut_with_page_size() {
        reset();

        let mut buf = BytesMut::with_page_size(BytePageSize::Size8);
        assert_eq!(buf.page_size(), BytePageSize::Size8);
        assert_eq!(buf.capacity(), BytePageSize::Size8.capacity());
        let ptr = buf.as_ptr();

        // split off data keeps the page until the last reference is dropped
        buf.extend_from_slice(&[1; 1000]);
        let head = buf.split_to(500);
        drop(buf);
        assert_eq!(cached_pages(BytePageSize::Size8), 0);
        drop(head);
        assert_eq!(cached_pages(BytePageSize::Size8), 1);

        let buf = BytesMut::with_page_size(BytePageSize::Size8);
        assert_eq!(buf.as_ptr(), ptr);
        assert_eq!(buf.capacity(), BytePageSize::Size8.capacity());
        assert_eq!(cached_pages(BytePageSize::Size8), 0);

        // `Unset` creates a `Size64` page
        let buf = BytesMut::with_page_size(BytePageSize::Unset);
        assert_eq!(buf.page_size(), BytePageSize::Size64);
        assert_eq!(buf.capacity(), BytePageSize::Size64.capacity());
        drop(buf);
        assert_eq!(cached_pages(BytePageSize::Size64), 1);

        // buffers without a page size are not cached
        let buf = BytesMut::with_capacity(BytePageSize::Size64.capacity() + 1);
        assert_eq!(buf.page_size(), BytePageSize::Unset);
        assert_eq!(BytesMut::new().page_size(), BytePageSize::Unset);
        assert_eq!(BytesMut::with_capacity(64).page_size(), BytePageSize::Unset);
        assert_eq!(
            BytesMut::copy_from_slice(b"hello").page_size(),
            BytePageSize::Unset
        );
        drop(buf);
        for size in crate::size::PAGE_SIZES {
            let expected = usize::from(size == BytePageSize::Size64);
            assert_eq!(cached_pages(size), expected);
        }
    }

    /// STATEMENT: A new page request with no cached page of its size takes a cached page one size smaller, and the metrics record the fallback.
    ///
    /// HOW:
    /// - Cache a `Size8` page, request `Size16`, and check the same `Size8` page is returned, counted as a `Size16` miss and fallback and a `Size8` hit.
    /// - Request `Size16` again with an empty cache and check a `Size16` page is allocated, counted as a miss only.
    /// - Check the totals include the fallback.
    #[test]
    fn new_page_falls_back_to_smaller_size() {
        reset();
        let metrics = PageCacheMetrics::current();
        let (s8, s16) = (BytePageSize::Size8, BytePageSize::Size16);

        let st = StorageVec::sized(s8);
        let addr = st.0;
        drop(st);

        let st = StorageVec::sized(s16);
        assert_eq!(st.0, addr);
        assert_eq!(st.page_size(), s8);
        assert_eq!(cached_pages(s8), 0);
        let snapshot = metrics.snapshot();
        let s = *snapshot.get(s16).unwrap();
        assert_eq!((s.hits, s.misses, s.fallbacks, s.pages), (0, 1, 1, 0));
        let s = *snapshot.get(s8).unwrap();
        assert_eq!(
            (s.hits, s.misses, s.fallbacks, s.cached, s.pages),
            (1, 1, 0, 1, 0)
        );

        let st2 = StorageVec::sized(s16);
        assert_eq!(st2.page_size(), s16);
        let snapshot = metrics.snapshot();
        let s = *snapshot.get(s16).unwrap();
        assert_eq!((s.hits, s.misses, s.fallbacks), (0, 2, 1));
        let total = snapshot.total();
        assert_eq!((total.hits, total.misses, total.fallbacks), (1, 3, 1));
        drop((st, st2));
        reset();
    }

    /// STATEMENT: The fallback goes exactly one page size down and only when the requested size has no cached page.
    ///
    /// HOW:
    /// - Cache a `Size4` page and check a `Size16` request allocates `Size16` and leaves the `Size4` page cached.
    /// - Cache `Size8` and `Size16` pages and check a `Size16` request takes the `Size16` page and leaves the `Size8` page cached.
    #[test]
    fn new_page_falls_back_one_size_only() {
        reset();
        let (s4, s8, s16) = (
            BytePageSize::Size4,
            BytePageSize::Size8,
            BytePageSize::Size16,
        );

        drop(StorageVec::sized_exact(s4));
        let st = StorageVec::sized(s16);
        assert_eq!(st.page_size(), s16);
        assert_eq!(cached_pages(s4), 1);

        let p8 = StorageVec::sized_exact(s8);
        drop(st);
        drop(p8);
        let st = StorageVec::sized(s16);
        assert_eq!(st.page_size(), s16);
        assert_eq!((cached_pages(s8), cached_pages(s16)), (1, 0));
        drop(st);
        reset();
    }

    /// STATEMENT: New `BytesMut` buffers and new `BytePages` pages take a cached page one size smaller, `Unset` falls back from `Size64` to `Size48`.
    ///
    /// HOW:
    /// - Cache a `Size8` page and check `BytesMut::with_page_size(Size16)` returns it with `Size8` capacity.
    /// - Cache a `Size48` page and check `BytesMut::with_page_size(Unset)` returns it.
    /// - With the `tracker` feature, check `BytesMut::with_page_size_in` falls back too.
    /// - Cache a `Size8` page, write to a `Size16` `BytePages`, and check the page left the cache as a `Size16` fallback.
    #[test]
    fn new_buffers_fall_back_to_smaller_page() {
        reset();
        let metrics = PageCacheMetrics::current();
        let (s8, s16) = (BytePageSize::Size8, BytePageSize::Size16);

        drop(StorageVec::sized_exact(s8));
        let buf = BytesMut::with_page_size(s16);
        assert_eq!(buf.page_size(), s8);
        assert_eq!(buf.capacity(), s8.capacity());
        drop(buf);

        drop(StorageVec::sized_exact(BytePageSize::Size48));
        let buf = BytesMut::with_page_size(BytePageSize::Unset);
        assert_eq!(buf.page_size(), BytePageSize::Size48);
        drop(buf);

        #[cfg(feature = "tracker")]
        {
            let tracker = BytesTracker::new();
            let buf = BytesMut::with_page_size_in(s16, &tracker);
            assert_eq!(buf.page_size(), s8);
            assert_eq!(tracker.snapshot().live_bytes, s8.alloc_size());
        }

        assert_eq!(cached_pages(s8), 1);
        let mut pages = BytePages::new(s16);
        pages.put_slice(b"hello");
        assert_eq!(cached_pages(s8), 0);
        let expected = if cfg!(feature = "tracker") { 3 } else { 2 };
        assert_eq!(metrics.snapshot().get(s16).unwrap().fallbacks, expected);
        drop(pages);
        reset();
    }
}
