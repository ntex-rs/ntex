//! Per-thread page cache and its statistics.
use std::cell::Cell;
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
    static CACHE: Cell<Option<Box<Cache>>> = Cell::new(Some(Box::default()));
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
            // the cache is taken out of the thread-local, the released pages
            // are freed instead of being cached again
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
        .try_with(|c| {
            let mut cst = c.take()?;
            let res = f(&mut cst);
            c.set(Some(cst));
            Some(res)
        })
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
pub(crate) fn reset() {
    CACHE.with(|c| c.set(Some(Box::default())));
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
    /// Number of pages taken from the cache.
    pub hits: u64,
    /// Number of pages allocated because the cache was empty.
    pub misses: u64,
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
        let cache = CACHE.with(Cell::take);
        drop(st);
        CACHE.with(|c| c.set(cache));
        assert_eq!(cached_pages(BytePageSize::Size16), 0);

        // the setting is ignored while the cache is in use
        let cache = CACHE.with(Cell::take);
        crate::set_pages_cache(3);
        CACHE.with(|c| c.set(cache));
        assert_eq!(cache_limits(), [16; CLASSES]);
    }

    fn cache_limits() -> [usize; CLASSES] {
        CACHE.with(|c| {
            let cst = c.take().unwrap();
            let limits = cst.limits;
            c.set(Some(cst));
            limits
        })
    }

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
        let cache = CACHE.with(Cell::take);
        crate::set_page_cache_size(BytePageSize::Size48, 3);
        CACHE.with(|c| c.set(cache));
        assert_eq!(cache_limits(), expected);

        crate::set_pages_cache(2);
        assert_eq!(cache_limits(), [2; CLASSES]);
        reset();
    }

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
}
