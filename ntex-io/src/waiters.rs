//! Tagged waker lists stored in a slab.
use std::{borrow::Cow, cell::Cell, pin::Pin, task::Context, task::Poll, task::Waker};

use ntex_util::HashMap;

use crate::{IoRef, utils::Extensions};

const NONE: u16 = u16::MAX;

/// Waker tag of the connection disconnect waiters.
pub(crate) const TAG_DISCONNECT: usize = usize::MAX - 1;
/// Waker tag of the write back-pressure waiters.
pub(crate) const TAG_WRITE: usize = usize::MAX - 2;

/// Panics in debug builds if the tag is reserved for internal use.
pub(crate) fn check_public_tag(tag: usize) {
    debug_assert!(tag < TAG_WRITE, "waker tag {tag} is reserved");
}

/// Identifier of a registered waker.
///
/// The identifier becomes stale once the entry is woken or removed, the slot
/// generation guards a reused slot against stale identifiers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct WaiterId {
    idx: u16,
    generation: u16,
}

/// Waker registration of a single waiter.
///
/// Holds the id of the registered entry, the entry is released on wake.
#[derive(Debug)]
pub(crate) struct WaiterEntry {
    pub(crate) tag: usize,
    pub(crate) id: Cell<Option<WaiterId>>,
}

impl WaiterEntry {
    pub(crate) const fn new(tag: usize) -> Self {
        Self {
            tag,
            id: Cell::new(None),
        }
    }

    fn take(&self) -> Self {
        Self {
            tag: self.tag,
            id: Cell::new(self.id.take()),
        }
    }
}

struct Entry {
    waker: Option<Waker>,
    prev: u16,
    next: u16,
    generation: u16,
}

/// Wakers grouped by tag.
///
/// Each tag has a doubly linked list of wakers, the entries are stored
/// in a slab like vector with a free list, released slots are reused.
/// Only tags with registered wakers are kept in the map. It holds up to
/// `u16::MAX` wakers, registration panics above that.
pub(crate) struct Waiters {
    free: u16,
    entries: Vec<Entry>,
    // list head of each non-empty tag
    tags: HashMap<usize, u16>,
}

impl Default for Waiters {
    fn default() -> Self {
        Self::new()
    }
}

impl Waiters {
    pub(crate) fn new() -> Self {
        Self {
            free: NONE,
            entries: Vec::new(),
            tags: HashMap::default(),
        }
    }

    /// Registers the waker for the tag.
    pub(crate) fn register(&mut self, tag: usize, waker: &Waker) -> WaiterId {
        let head = self.tags.get(&tag).copied().unwrap_or(NONE);

        let idx = if self.free == NONE {
            let idx = u16::try_from(self.entries.len()).expect("too many wakers");
            assert!(idx != NONE, "too many wakers");
            self.entries.push(Entry {
                waker: Some(waker.clone()),
                generation: 0,
                prev: NONE,
                next: head,
            });
            idx
        } else {
            let idx = self.free;
            let entry = &mut self.entries[idx as usize];
            self.free = entry.next;
            entry.waker = Some(waker.clone());
            entry.prev = NONE;
            entry.next = head;
            idx
        };

        if head != NONE {
            self.entries[head as usize].prev = idx;
        }
        self.tags.insert(tag, idx);
        WaiterId {
            idx,
            generation: self.entries[idx as usize].generation,
        }
    }

    /// Replaces the waker of a registered entry.
    ///
    /// Returns `false` if the entry is woken or removed.
    pub(crate) fn update(&mut self, id: WaiterId, waker: &Waker) -> bool {
        if let Some(entry) = self.get(id)
            && let Some(ref mut w) = entry.waker
        {
            w.clone_from(waker);
            true
        } else {
            false
        }
    }

    /// Removes a registered entry, a woken or removed entry is ignored.
    ///
    /// The tag must be the one the entry is registered with, otherwise
    /// the tag lists are corrupted.
    pub(crate) fn remove(&mut self, id: WaiterId, tag: usize) {
        let Some(entry) = self.get(id) else {
            return;
        };
        let (prev, next) = (entry.prev, entry.next);
        if prev != NONE {
            self.entries[prev as usize].next = next;
        } else if next == NONE {
            self.tags.remove(&tag);
        } else {
            self.tags.insert(tag, next);
        }
        if next != NONE {
            self.entries[next as usize].prev = prev;
        }
        drop(self.release(id.idx));
    }

    /// Wakes and removes all entries.
    pub(crate) fn wake_all(&mut self) {
        while let Some(&tag) = self.tags.keys().next() {
            self.wake(tag);
        }
    }

    /// Wakes and removes all entries of the tag.
    pub(crate) fn wake(&mut self, tag: usize) {
        let Some(mut idx) = self.tags.remove(&tag) else {
            return;
        };

        while idx != NONE {
            let next = self.entries[idx as usize].next;
            if let Some(waker) = self.release(idx) {
                waker.wake();
            }
            idx = next;
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.iter().filter(|e| e.waker.is_some()).count()
    }

    #[cfg(test)]
    fn is_registered(&self, id: WaiterId) -> bool {
        self.entries
            .get(id.idx as usize)
            .is_some_and(|e| e.generation == id.generation && e.waker.is_some())
    }

    fn get(&mut self, id: WaiterId) -> Option<&mut Entry> {
        self.entries
            .get_mut(id.idx as usize)
            .filter(|e| e.generation == id.generation && e.waker.is_some())
    }

    /// Moves the slot to the free list, the caller unlinks it from the tag list.
    fn release(&mut self, idx: u16) -> Option<Waker> {
        let entry = &mut self.entries[idx as usize];
        entry.generation = entry.generation.wrapping_add(1);
        entry.prev = NONE;
        entry.next = self.free;
        self.free = idx;
        entry.waker.take()
    }
}

/// Removes the registered waker on drop.
pub(crate) struct WriteGuard<'a> {
    ext: &'a Extensions,
    slot: WaiterEntry,
}

impl<'a> WriteGuard<'a> {
    pub(crate) fn new(ext: &'a Extensions) -> Self {
        Self {
            ext,
            slot: WaiterEntry::new(TAG_WRITE),
        }
    }

    pub(crate) fn register(&self, cx: &mut Context<'_>) {
        self.ext.register_waker(&self.slot, cx.waker());
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        self.ext.remove_waker(&self.slot);
    }
}

/// A waiter registered for a tag of the I/O stream.
///
/// The waiter and [`poll_ready`](Self::poll_ready) complete once
/// [`IoRef::wake`] is called for the tag, all waiters of the tag are woken
/// together. They also complete once the I/O stream is closed. The first poll
/// registers the waiter, a wake is reported to a registered waiter even if it
/// happens between polls, once reported the next poll registers again. A
/// waiter that is not registered misses the wake. Dropping the waiter releases
/// its registration.
#[derive(Debug)]
#[must_use = "a waiter does nothing unless polled"]
pub struct Waiter<'a> {
    io: Cow<'a, IoRef>,
    waiter: WaiterEntry,
}

impl<'a> Waiter<'a> {
    /// Creates a waiter for the tag.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if the tag is reserved for internal use,
    /// `usize::MAX - 1` and `usize::MAX - 2` are reserved.
    pub fn new(io: &'a IoRef, tag: usize) -> Self {
        check_public_tag(tag);
        Self {
            io: Cow::Borrowed(io),
            waiter: WaiterEntry::new(tag),
        }
    }

    pub(crate) fn new_static(io: IoRef, tag: usize) -> Self {
        Self {
            io: Cow::Owned(io),
            waiter: WaiterEntry::new(tag),
        }
    }

    /// Polls until the tag is woken.
    ///
    /// Completes once the I/O stream is closed.
    pub fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<()> {
        let st = &self.io.0;
        if st.flags.is_closed() {
            Poll::Ready(())
        } else {
            st.extensions.poll_waker(&self.waiter, cx.waker())
        }
    }

    /// Converts the waiter into one that owns its [`IoRef`].
    ///
    /// The registration of the waiter is kept.
    pub fn into_static(self) -> Waiter<'static> {
        let io = Cow::Owned(IoRef::clone(&self.io));

        Waiter {
            io,
            waiter: self.waiter.take(),
        }
    }
}

impl Clone for Waiter<'_> {
    /// Creates an unregistered waiter for the same tag.
    fn clone(&self) -> Self {
        Self {
            io: self.io.clone(),
            waiter: WaiterEntry::new(self.waiter.tag),
        }
    }
}

impl Future for Waiter<'_> {
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.poll_ready(cx)
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.io.0.extensions.remove_waker(&self.waiter);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
    use std::task::{Wake, Waker};

    use super::*;

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn waker() -> (Arc<Counter>, Waker) {
        let cnt = Arc::new(Counter(AtomicUsize::new(0)));
        (cnt.clone(), Waker::from(cnt))
    }

    fn count(cnt: &Counter) -> usize {
        cnt.0.load(Ordering::Relaxed)
    }

    #[test]
    fn wake_by_tag() {
        let mut wakers = Waiters::new();
        let (c1, w1) = waker();
        let (c2, w2) = waker();
        let (c3, w3) = waker();

        let id1 = wakers.register(0, &w1);
        let id2 = wakers.register(1, &w2);
        let id3 = wakers.register(0, &w3);

        wakers.wake(0);
        assert_eq!((count(&c1), count(&c2), count(&c3)), (1, 0, 1));
        assert!(!wakers.is_registered(id1));
        assert!(wakers.is_registered(id2));
        assert!(!wakers.is_registered(id3));

        // woken entries are removed
        wakers.wake(0);
        assert_eq!((count(&c1), count(&c3)), (1, 1));

        wakers.wake(1);
        assert_eq!(count(&c2), 1);
        assert!(!wakers.is_registered(id2));
        assert_eq!(wakers.entries.len(), 3);
        assert_eq!(wakers.len(), 0);
    }

    #[test]
    fn dynamic_tags() {
        let mut wakers = Waiters::default();
        let (cnt, w) = waker();

        // unknown tag
        wakers.wake(7);
        assert!(wakers.tags.is_empty());

        let id = wakers.register(200, &w);
        assert_eq!(wakers.tags.len(), 1);
        let id3 = wakers.register(3, &w);
        wakers.register(3, &w);
        assert_eq!(wakers.tags.len(), 2);

        wakers.wake(3);
        assert_eq!(count(&cnt), 2);
        assert!(wakers.is_registered(id));
        assert_eq!(wakers.tags.len(), 1);

        // removing the last entry drops the tag
        wakers.remove(id3, 3);
        wakers.remove(id, 200);
        assert!(wakers.tags.is_empty());
        wakers.wake(200);
        assert_eq!(count(&cnt), 2);

        wakers.register(3, &w);
        let id3 = wakers.register(3, &w);
        wakers.remove(id3, 3);
        assert_eq!(wakers.tags.len(), 1);
        wakers.wake(3);
        assert_eq!(count(&cnt), 3);
        assert!(wakers.tags.is_empty());
    }

    #[test]
    fn capacity() {
        let mut wakers = Waiters::new();
        let (_, w) = waker();

        let ids: Vec<_> = (0..u16::MAX).map(|_| wakers.register(0, &w)).collect();
        assert_eq!(wakers.len(), usize::from(u16::MAX));

        // a released slot is reused at capacity
        wakers.remove(ids[100], 0);
        assert_eq!(wakers.register(0, &w).idx, 100);

        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wakers.register(0, &w);
        }));
        assert!(res.is_err());
    }

    #[test]
    fn remove() {
        let mut wakers = Waiters::new();
        let (cnt, w) = waker();

        // head, middle and tail of the list
        let ids: Vec<_> = (0..5).map(|_| wakers.register(0, &w)).collect();
        wakers.remove(ids[4], 0);
        wakers.remove(ids[2], 0);
        wakers.remove(ids[0], 0);
        assert_eq!(wakers.len(), 2);

        // a removed entry is ignored while the slot is free
        wakers.remove(ids[0], 0);
        assert_eq!(wakers.len(), 2);

        wakers.wake(0);
        assert_eq!(count(&cnt), 2);
        assert_eq!(wakers.len(), 0);
    }

    #[test]
    fn update() {
        let mut wakers = Waiters::new();
        let (c1, w1) = waker();
        let (c2, w2) = waker();

        let id = wakers.register(0, &w1);
        assert!(wakers.update(id, &w2));
        wakers.wake(0);
        assert_eq!((count(&c1), count(&c2)), (0, 1));
        assert!(!wakers.update(id, &w1));
    }

    #[test]
    fn wake_all() {
        let mut wakers = Waiters::new();
        let (cnt, w) = waker();

        wakers.wake_all();
        for tag in [0, 1, 5, 5] {
            wakers.register(tag, &w);
        }
        wakers.wake_all();
        assert_eq!(count(&cnt), 4);
        assert!(wakers.tags.is_empty());
        assert_eq!(wakers.len(), 0);
    }

    #[test]
    fn stale_ids() {
        let mut wakers = Waiters::new();
        let (c1, w1) = waker();
        let (c2, w2) = waker();

        let stale = wakers.register(0, &w1);
        wakers.wake(0);
        assert_eq!(count(&c1), 1);

        // the slot is reused, the stale id does not touch the new entry
        let id = wakers.register(0, &w2);
        assert_eq!(id.idx, stale.idx);
        assert!(!wakers.is_registered(stale));
        assert!(!wakers.update(stale, &w1));
        wakers.remove(stale, 0);
        assert!(wakers.is_registered(id));

        wakers.wake(0);
        assert_eq!((count(&c1), count(&c2)), (1, 1));
    }

    #[test]
    fn free_list_reuse() {
        let mut wakers = Waiters::new();
        let (cnt, w) = waker();

        let a: Vec<_> = (0..4).map(|i| wakers.register(i % 2, &w)).collect();
        wakers.wake(0);
        wakers.remove(a[1], 1);

        // three released slots are reused
        let b: Vec<_> = (0..3).map(|_| wakers.register(1, &w)).collect();
        assert_eq!(wakers.entries.len(), 4);
        for id in &b {
            assert!(wakers.is_registered(*id));
        }
        assert!(wakers.is_registered(a[3]));

        wakers.wake(1);
        assert_eq!(count(&cnt), 2 + 4);
        assert_eq!(wakers.len(), 0);
    }
}
