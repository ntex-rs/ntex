//! Page cache statistics.
use std::sync::{Arc, atomic::AtomicU64, atomic::Ordering::Relaxed};

use crate::{BytePageSize, size::PAGE_SIZES};

const CLASSES: usize = PAGE_SIZES.len();

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
        crate::stvec::cache_metrics().unwrap_or_default()
    }

    /// Installs `metrics` as the metrics of the current thread's page cache.
    ///
    /// The pages held by the cache are moved from the previous metrics to
    /// `metrics`. Metrics must not be current on more than one thread at a
    /// time, updates are not atomic read-modify-writes.
    pub fn set_current(metrics: Arc<Self>) {
        crate::stvec::set_cache_metrics(metrics);
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
