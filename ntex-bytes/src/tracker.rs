//! Memory attribution for buffer allocations.
use std::sync::atomic::{AtomicUsize, Ordering::Acquire, Ordering::Relaxed, Ordering::Release};
use std::{fmt, ptr::NonNull, sync::atomic::fence};

use crate::stvec::abort;

const MAX_REFS: usize = isize::MAX as usize;

/// Shared counter of the memory held by buffer allocations.
///
/// An allocation attributed to a tracker charges its size, including the
/// allocation header, until the allocation is freed or returned to the
/// page cache. The tracker state lives while any handle or attributed
/// allocation exists, so a handle can outlive the buffers and the
/// buffers can outlive all handles.
// `BytesMut::tracker` borrows the allocation header field as a handle
#[cfg_attr(docsrs, doc(cfg(feature = "tracker")))]
#[repr(transparent)]
pub struct BytesTracker(NonNull<TrackerInner>);

const _: () = assert!(size_of::<BytesTracker>() == size_of::<*const TrackerInner>());

/// Point-in-time values of a [`BytesTracker`].
///
/// The counters are read one by one, the snapshot is exact only when
/// no allocation is attributed or released concurrently.
#[cfg_attr(docsrs, doc(cfg(feature = "tracker")))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BytesTrackerSnapshot {
    /// Memory held by attributed allocations, in bytes.
    pub live_bytes: usize,
    /// Number of attributed allocations.
    pub live_allocations: usize,
}

pub(crate) struct TrackerInner {
    // owner handles + attributed allocations
    refs: AtomicUsize,
    // owner handles
    handles: AtomicUsize,
    live_bytes: AtomicUsize,
}

// SAFETY: the shared state consists of atomics only
unsafe impl Send for BytesTracker {}
// SAFETY: the shared state consists of atomics only
unsafe impl Sync for BytesTracker {}

impl BytesTracker {
    /// Creates a new tracker.
    pub fn new() -> Self {
        let inner = Box::new(TrackerInner {
            refs: AtomicUsize::new(1),
            handles: AtomicUsize::new(1),
            live_bytes: AtomicUsize::new(0),
        });
        BytesTracker(NonNull::from(Box::leak(inner)))
    }

    /// Returns the current values of the tracker.
    pub fn snapshot(&self) -> BytesTrackerSnapshot {
        let inner = self.inner();
        let handles = inner.handles.load(Relaxed);
        let refs = inner.refs.load(Relaxed);
        BytesTrackerSnapshot {
            live_bytes: inner.live_bytes.load(Relaxed),
            // fields are read one by one, a racing clone can make it negative
            live_allocations: refs.saturating_sub(handles),
        }
    }

    /// Returns `true` if both handles refer to the same tracker.
    pub fn ptr_eq(&self, other: &BytesTracker) -> bool {
        self.0 == other.0
    }

    pub(crate) fn as_inner(&self) -> *const TrackerInner {
        self.0.as_ptr()
    }

    fn inner(&self) -> &TrackerInner {
        // SAFETY: the handle holds a reference
        unsafe { self.0.as_ref() }
    }
}

impl Default for BytesTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for BytesTracker {
    fn clone(&self) -> Self {
        let inner = self.inner();
        if inner.refs.fetch_add(1, Relaxed) > MAX_REFS {
            abort();
        }
        inner.handles.fetch_add(1, Relaxed);
        BytesTracker(self.0)
    }
}

impl Drop for BytesTracker {
    fn drop(&mut self) {
        self.inner().handles.fetch_sub(1, Relaxed);
        // SAFETY: the handle holds a reference
        unsafe { TrackerInner::release(self.0.as_ptr()) }
    }
}

impl fmt::Debug for BytesTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let snapshot = self.snapshot();
        f.debug_struct("BytesTracker")
            .field("live_bytes", &snapshot.live_bytes)
            .field("live_allocations", &snapshot.live_allocations)
            .finish()
    }
}

impl TrackerInner {
    /// Attributes an allocation of `size` bytes, it holds a reference
    /// until [`TrackerInner::uncharge`].
    pub(crate) fn charge(&self, size: usize) {
        if self.refs.fetch_add(1, Relaxed) > MAX_REFS {
            abort();
        }
        self.live_bytes.fetch_add(size, Relaxed);
    }

    /// Adjusts the size of an attributed allocation.
    pub(crate) fn resize(&self, old: usize, new: usize) {
        if new > old {
            self.live_bytes.fetch_add(new - old, Relaxed);
        } else {
            self.live_bytes.fetch_sub(old - new, Relaxed);
        }
    }

    /// Releases an allocation of `size` bytes.
    ///
    /// # Safety
    ///
    /// `this` must point to a live tracker and the caller must own
    /// a reference taken by [`TrackerInner::charge`].
    pub(crate) unsafe fn uncharge(this: *const TrackerInner, size: usize) {
        // SAFETY: the caller owns a reference
        unsafe { (*this).live_bytes.fetch_sub(size, Relaxed) };
        // SAFETY: the caller owns a reference
        unsafe { TrackerInner::release(this) }
    }

    /// # Safety
    ///
    /// The caller must own a reference, it is consumed.
    unsafe fn release(this: *const TrackerInner) {
        // SAFETY: the caller owns a reference
        if unsafe { (*this).refs.fetch_sub(1, Release) } != 1 {
            return;
        }
        fence(Acquire);
        // SAFETY: last reference, the state was created by Box in BytesTracker::new
        drop(unsafe { Box::from_raw(this.cast_mut()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(bytes: usize, allocs: usize) -> BytesTrackerSnapshot {
        BytesTrackerSnapshot {
            live_bytes: bytes,
            live_allocations: allocs,
        }
    }

    /// STATEMENT: Tracker charges and resizes update live bytes while allocation counts follow allocation lifetimes.
    ///
    /// HOW:
    /// - Charge 100 and 50 bytes, grow and shrink one charge, and check snapshots after each operation.
    /// - Uncharge both allocations and verify zero totals and the empty debug representation.
    #[test]
    fn charge_resize_uncharge() {
        let tracker = BytesTracker::new();
        assert_eq!(tracker.snapshot(), snap(0, 0));

        let inner = tracker.as_inner();
        // SAFETY: the tracker is alive
        unsafe { (*inner).charge(100) };
        // SAFETY: The tracker handle keeps `inner` alive while this additional allocation is
        // charged.
        unsafe { (*inner).charge(50) };
        assert_eq!(tracker.snapshot(), snap(150, 2));

        // SAFETY: The tracker handle keeps `inner` alive while its existing charge is resized.
        unsafe { (*inner).resize(100, 300) };
        assert_eq!(tracker.snapshot(), snap(350, 2));
        // SAFETY: The tracker handle keeps `inner` alive while its existing charge is resized.
        unsafe { (*inner).resize(300, 200) };
        // SAFETY: The tracker handle keeps `inner` alive while its existing charge is resized.
        unsafe { (*inner).resize(200, 200) };
        assert_eq!(tracker.snapshot(), snap(250, 2));

        // SAFETY: The first charge retains a reference and now accounts for 200 bytes.
        unsafe { TrackerInner::uncharge(inner, 200) };
        assert_eq!(tracker.snapshot(), snap(50, 1));
        // SAFETY: The second charge retains a reference and accounts for 50 bytes.
        unsafe { TrackerInner::uncharge(inner, 50) };
        assert_eq!(tracker.snapshot(), snap(0, 0));
        assert_eq!(
            format!("{tracker:?}"),
            "BytesTracker { live_bytes: 0, live_allocations: 0 }"
        );
    }

    /// STATEMENT: Cloned tracker handles share accounting without counting handles as allocations.
    ///
    /// HOW:
    /// - Compare tracker identities, charge ten bytes, and observe the charge through a clone.
    /// - Drop the clone, check the charge persists, and uncharge to return the original snapshot to zero.
    #[test]
    fn handles() {
        let tracker = BytesTracker::default();
        let tracker2 = tracker.clone();
        assert!(tracker.ptr_eq(&tracker2));
        assert!(!tracker.ptr_eq(&BytesTracker::new()));

        // handles are not allocations
        assert_eq!(tracker.snapshot(), snap(0, 0));
        // SAFETY: The tracker handle keeps its state alive while the allocation is charged.
        unsafe { (*tracker.as_inner()).charge(10) };
        assert_eq!(tracker2.snapshot(), snap(10, 1));
        drop(tracker2);
        assert_eq!(tracker.snapshot(), snap(10, 1));
        // SAFETY: The ten-byte charge retains the reference consumed by this uncharge.
        unsafe { TrackerInner::uncharge(tracker.as_inner(), 10) };
        assert_eq!(tracker.snapshot(), snap(0, 0));
    }

    /// STATEMENT: Outstanding allocation charges keep tracker state alive after all public handles drop.
    ///
    /// HOW:
    /// - Charge ten and twenty bytes, drop the tracker handle, and release both charges through the saved inner pointer.
    #[test]
    fn allocation_outlives_handles() {
        let tracker = BytesTracker::new();
        let inner = tracker.as_inner();
        // SAFETY: The tracker handle keeps `inner` alive while the allocation is charged.
        unsafe { (*inner).charge(10) };
        // SAFETY: The tracker handle keeps `inner` alive while the additional allocation is
        // charged.
        unsafe { (*inner).charge(20) };
        drop(tracker);
        // the allocations keep the state alive, the last one frees it
        // SAFETY: The ten-byte charge keeps `inner` alive and supplies the reference being
        // released.
        unsafe { TrackerInner::uncharge(inner, 10) };
        // SAFETY: The twenty-byte charge keeps `inner` alive and supplies the final reference being
        // released.
        unsafe { TrackerInner::uncharge(inner, 20) };
    }

    /// STATEMENT: Concurrent tracker operations retain accurate totals and allow charges to outlive thread handles.
    ///
    /// HOW:
    /// - Run four threads repeatedly cloning handles, charging, resizing, and uncharging allocations.
    /// - Join them, verify one byte and allocation per thread remain, then drop the last handle and release those charges.
    #[test]
    fn concurrent() {
        const THREADS: usize = 4;
        const ITERS: usize = if cfg!(miri) { 20 } else { 10_000 };

        let tracker = BytesTracker::new();
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let tracker = tracker.clone();
                std::thread::spawn(move || {
                    let inner = tracker.as_inner();
                    for i in 0..ITERS {
                        let handle = tracker.clone();
                        // SAFETY: The thread owns a tracker handle that keeps `inner` alive while
                        // charging.
                        unsafe { (*inner).charge(i) };
                        // SAFETY: The thread owns a tracker handle and resizes its own existing
                        // charge.
                        unsafe { (*inner).resize(i, i + 1) };
                        drop(handle);
                        // SAFETY: The loop iteration owns the charge and its reference, resized
                        // above to `i + 1` bytes.
                        unsafe { TrackerInner::uncharge(inner, i + 1) };
                    }
                    // left for the main thread to release
                    // SAFETY: The thread still owns a tracker handle; the new charge retains a
                    // reference for the main thread.
                    unsafe { (*inner).charge(1) };
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(tracker.snapshot(), snap(THREADS, THREADS));

        let inner = tracker.as_inner();
        drop(tracker);
        for _ in 0..THREADS {
            // SAFETY: Each joined thread left one one-byte charge and a reference, consumed once
            // per iteration.
            unsafe { TrackerInner::uncharge(inner, 1) };
        }
    }
}
