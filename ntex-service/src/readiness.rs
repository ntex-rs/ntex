use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    future::{Future, poll_fn},
    pin::pin,
    rc::{Rc, Weak},
    sync::{Arc, Mutex},
    task::{Poll, Wake, Waker},
};

/// A source of readiness-change notifications.
///
/// A notification invalidates the cached readiness of subscribed scopes and
/// their dependent ancestors before waking any observers. It requests another
/// readiness check; it does not grant readiness or run or drop readiness futures.
///
/// Clones share the same notification source. [`Default`] and [`new`](Self::new)
/// each create an independent source. Subscriptions belong to readiness scopes
/// and persist until the scope or source is dropped, even after a readiness
/// check completes or is cancelled.
///
/// Notify after any change that can affect readiness, including consuming the
/// last available slot and releasing capacity. The change and notification must
/// happen before another caller can use cached readiness.
///
/// This type is neither `Send` nor `Sync`.
#[derive(Clone, Debug, Default)]
pub struct ReadyNotifier(Rc<SourceInner>);

impl ReadyNotifier {
    /// Creates an independent notification source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Invalidates all subscribed readiness scopes and their ancestors, then
    /// wakes their observers.
    ///
    /// Every notification advances the affected scopes' generations, including
    /// scopes that are already invalid or have a pending readiness check.
    pub fn notify(&self) {
        let mut affected: Vec<_> = self
            .0
            .scopes
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let direct = affected.len();
        let mut visited: HashSet<_, foldhash::fast::RandomState> =
            affected.iter().map(Rc::as_ptr).collect();

        let mut idx = 0;
        while idx < affected.len() {
            let scope = Rc::clone(&affected[idx]);
            scope.invalidate(idx < direct);
            for parent in scope.parents.borrow().iter().filter_map(Weak::upgrade) {
                if visited.insert(Rc::as_ptr(&parent)) {
                    affected.push(parent);
                }
            }
            idx += 1;
        }

        // Drain the entire graph before a wake can reenter and register again.
        let mut wakers = Vec::new();
        for scope in &affected {
            scope.take_wakers(&mut wakers);
        }
        for waker in wakers {
            waker.wake();
        }
    }
}

#[derive(Debug, Default)]
struct SourceInner {
    scopes: RefCell<Vec<Weak<ScopeInner>>>,
}

impl Drop for SourceInner {
    fn drop(&mut self) {
        let ptr = std::ptr::from_ref(self);
        for scope in self.scopes.get_mut().drain(..).filter_map(|w| w.upgrade()) {
            scope.sources.borrow_mut().retain(|w| w.as_ptr() != ptr);
        }
    }
}

/// A snapshot for one scope's readiness check, not a transferable readiness permit.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Generation {
    value: u64,
    direct: u64,
}

/// A stable, shared readiness domain. Cloning aliases the same cache and graph.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReadyScope(Rc<ScopeInner>);

impl ReadyScope {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn generation(&self) -> Generation {
        self.0.generation.get()
    }

    pub(crate) fn is_current(&self, generation: Generation) -> bool {
        self.generation() == generation
    }

    /// Only direct invalidation requires restarting an outstanding check.
    pub(crate) fn is_directly_invalidated(&self, generation: Generation) -> bool {
        self.generation().direct != generation.direct
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.0.ready.get()
    }

    pub(crate) fn reset_root(&self) {
        self.0.invalidate(true);
        self.wake_observers();
    }

    /// Records a successful check only if no dependency changed while checking.
    pub(crate) fn set_ready(&self, generation: Generation) -> bool {
        if self.is_current(generation) {
            self.0.ready.set(true);
            true
        } else {
            false
        }
    }

    /// Subscribes before inspecting the resource's availability.
    pub(crate) fn subscribe(&self, source: &ReadyNotifier) {
        let ptr = Rc::as_ptr(&source.0);
        let mut sources = self.0.sources.borrow_mut();
        if !sources.iter().any(|w| w.as_ptr() == ptr) {
            sources.push(Rc::downgrade(&source.0));
            source.0.scopes.borrow_mut().push(Rc::downgrade(&self.0));
        }
    }

    /// Registers the dependency before reading the child's cache, even on a hit.
    pub(crate) fn depend_on(&self, child: &Self) {
        if Rc::ptr_eq(&self.0, &child.0) {
            return;
        }
        let ptr = Rc::as_ptr(&child.0);
        let mut children = self.0.children.borrow_mut();
        if !children.iter().any(|w| w.as_ptr() == ptr) {
            children.push(Rc::downgrade(&child.0));
            child.0.parents.borrow_mut().push(Rc::downgrade(&self.0));
        }
    }

    pub(crate) fn observer(&self) -> ReadyObserver {
        ReadyObserver {
            scope: Rc::downgrade(&self.0),
            idx: self.0.observers.0.lock().unwrap().insert(None),
        }
    }

    pub(crate) fn checker_waker(&self) -> Waker {
        Waker::from(Arc::new(CheckerWake(Arc::downgrade(&self.0.observers))))
    }

    /// Drives or joins a shared check without allocating on a cached success.
    pub(crate) async fn check<F, Fut, E>(&self, mut check: F) -> Result<(), E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        loop {
            if self.is_ready() {
                return Ok(());
            }

            // Cancellation drops the check, then its observer, then ownership.
            let mut owner = None;
            let observer = self.observer();
            poll_fn(|cx| {
                observer.register(cx.waker());
                if self.is_ready() {
                    Poll::Ready(())
                } else if self.0.checking.replace(true) {
                    Poll::Pending
                } else {
                    owner = Some(CheckOwner(self));
                    Poll::Ready(())
                }
            })
            .await;

            let result = if owner.is_some() {
                self.run_check(&mut check).await.map(|generation| {
                    self.set_ready(generation);
                })
            } else {
                Ok(())
            };
            drop(observer);
            drop(owner);
            result?;
            // Cleanup and handoff can reenter and invalidate the success.
        }
    }

    /// Checks raw services without borrowing the enclosing scope's cached success.
    pub(crate) async fn check_uncached<F, Fut, E>(&self, mut check: F) -> Result<Generation, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        self.run_check(&mut check).await
    }

    async fn run_check<F, Fut, E>(&self, check: &mut F) -> Result<Generation, E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let mut observer: Option<ReadyObserver> = None;
        loop {
            let generation = self.generation();
            let result = {
                let mut fut = pin!(check());
                poll_fn(|cx| {
                    if let Some(observer) = &observer {
                        observer.register(cx.waker());
                    }
                    if self.is_directly_invalidated(generation) {
                        return Poll::Ready(None);
                    }
                    let before_poll = self.generation();
                    let result = fut.as_mut().poll(cx);
                    if result.is_pending() {
                        observer
                            .get_or_insert_with(|| self.observer())
                            .register(cx.waker());
                        // A first-poll notification may precede registration.
                        if !self.is_current(before_poll) {
                            cx.waker().wake_by_ref();
                        }
                    }
                    result.map(Some)
                })
                .await
            };
            match result {
                Some(Err(err)) => return Err(err),
                Some(Ok(())) => {
                    drop(observer.take());
                    if self.is_current(generation) {
                        return Ok(generation);
                    }
                }
                None => {}
            }
        }
    }

    /// Schedules observers without changing cached readiness or generations.
    pub(crate) fn wake_observers(&self) {
        let mut wakers = Vec::new();
        self.0.take_wakers(&mut wakers);
        for waker in wakers {
            waker.wake();
        }
    }
}

#[derive(Debug, Default)]
struct ScopeInner {
    generation: Cell<Generation>,
    ready: Cell<bool>,
    checking: Cell<bool>,
    sources: RefCell<Vec<Weak<SourceInner>>>,
    parents: RefCell<Vec<Weak<Self>>>,
    children: RefCell<Vec<Weak<Self>>>,
    observers: Arc<ReadyObservers>,
}

// Readiness scopes are local, but a future may send its waker to another thread.
#[derive(Debug, Default)]
struct ReadyObservers(Mutex<slab::Slab<Option<Waker>>>);

impl ReadyObservers {
    fn take_wakers(&self, wakers: &mut Vec<Waker>) {
        for (_, waker) in self.0.lock().unwrap().iter_mut() {
            wakers.extend(waker.take());
        }
    }
}

struct CheckerWake(std::sync::Weak<ReadyObservers>);

impl Wake for CheckerWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(observers) = self.0.upgrade() {
            let mut wakers = Vec::new();
            observers.take_wakers(&mut wakers);
            for waker in wakers {
                waker.wake();
            }
        }
    }
}

struct CheckOwner<'a>(&'a ReadyScope);

impl Drop for CheckOwner<'_> {
    fn drop(&mut self) {
        self.0.0.checking.set(false);
        self.0.wake_observers();
    }
}

impl ScopeInner {
    fn invalidate(&self, direct: bool) {
        let mut generation = self.generation.get();
        generation.value = generation
            .value
            .checked_add(1)
            .expect("readiness generation exhausted");
        if direct {
            generation.direct = generation.value;
        }
        self.generation.set(generation);
        self.ready.set(false);
    }

    fn take_wakers(&self, wakers: &mut Vec<Waker>) {
        self.observers.take_wakers(wakers);
    }
}

impl Drop for ScopeInner {
    fn drop(&mut self) {
        let ptr = std::ptr::from_ref(self);
        for source in self.sources.get_mut().drain(..).filter_map(|w| w.upgrade()) {
            source.scopes.borrow_mut().retain(|w| w.as_ptr() != ptr);
        }
        for child in self
            .children
            .get_mut()
            .drain(..)
            .filter_map(|w| w.upgrade())
        {
            child.parents.borrow_mut().retain(|w| w.as_ptr() != ptr);
        }
        for parent in self.parents.get_mut().drain(..).filter_map(|w| w.upgrade()) {
            parent.children.borrow_mut().retain(|w| w.as_ptr() != ptr);
        }
    }
}

/// One task's registration, independent of the scope's persistent subscriptions.
#[derive(Debug)]
pub(crate) struct ReadyObserver {
    scope: Weak<ScopeInner>,
    idx: usize,
}

impl ReadyObserver {
    pub(crate) fn clear(&self) {
        if let Some(scope) = self.scope.upgrade() {
            let waker = scope.observers.0.lock().unwrap()[self.idx].take();
            drop(waker);
        }
    }

    /// Registers before checking readiness; returns false if the scope is gone.
    pub(crate) fn register(&self, waker: &Waker) -> bool {
        let Some(scope) = self.scope.upgrade() else {
            return false;
        };
        if scope.observers.0.lock().unwrap()[self.idx]
            .as_ref()
            .is_some_and(|prev| prev.will_wake(waker))
        {
            return true;
        }

        // Waker clone/drop implementations can reenter, just like wake.
        let waker = waker.clone();
        let previous = scope.observers.0.lock().unwrap()[self.idx].replace(waker);
        drop(previous);
        true
    }
}

impl Drop for ReadyObserver {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.upgrade() {
            let waker = scope.observers.0.lock().unwrap().remove(self.idx);
            drop(waker);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Wake},
    };

    use super::*;

    thread_local! {
        static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
        static ON_DROP: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
    }

    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Counter {
        fn wakes(&self) -> usize {
            self.0.load(Ordering::Relaxed)
        }
    }

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
            let hook = ON_WAKE.with_borrow_mut(Option::take);
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    struct DropHook;

    // Waker::noop() cannot exercise a reentrant waker destructor.
    #[allow(clippy::manual_noop_waker)]
    impl Wake for DropHook {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for DropHook {
        fn drop(&mut self) {
            let hook = ON_DROP.with_borrow_mut(Option::take);
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    #[test]
    fn first_poll_notification_is_not_lost_before_observer_registration() {
        let source = ReadyNotifier::new();
        let scope = ReadyScope::new();
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let first_poll = Cell::new(true);
        let mut check = pin!(scope.check_uncached(|| {
            scope.subscribe(&source);
            poll_fn(|_| {
                if first_poll.replace(false) {
                    source.notify();
                    Poll::Pending
                } else {
                    Poll::Ready(Ok::<_, ()>(()))
                }
            })
        }));
        assert!(check.as_mut().poll(&mut cx).is_pending());
        assert!(counter.wakes() > 0);
        assert_eq!(
            check.as_mut().poll(&mut cx),
            Poll::Ready(Ok(scope.generation()))
        );
        assert!(
            !scope.is_ready(),
            "raw checks do not publish cached success"
        );
    }

    #[test]
    fn reentrant_handoff_invalidation_rechecks_before_returning_success() {
        let source = ReadyNotifier::new();
        let scope = ReadyScope::new();
        scope.subscribe(&source);
        let observer = scope.observer();
        let waker = Waker::from(Arc::new(Counter::default()));
        assert!(observer.register(&waker));
        ON_WAKE.with_borrow_mut(|slot| {
            *slot = Some(Box::new(move || source.notify()));
        });
        let checks = Cell::new(0);
        let mut check = pin!(scope.check(|| {
            checks.set(checks.get() + 1);
            std::future::ready(Ok::<_, ()>(()))
        }));
        assert_eq!(
            check.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        );
        assert_eq!(checks.get(), 2);
        assert!(scope.is_ready());
    }

    #[test]
    fn persistent_subscriptions_and_generation_checks() {
        let source = ReadyNotifier::new();
        let alias = source.clone();
        let independent = ReadyNotifier::default();
        let scope = ReadyScope::new();
        let scope_alias = scope.clone();
        assert!(!scope.is_ready());
        scope.subscribe(&source);
        scope_alias.subscribe(&alias);
        assert_eq!(source.0.scopes.borrow().len(), 1);
        assert_eq!(scope.0.sources.borrow().len(), 1);

        let first = scope.generation();
        assert!(scope.set_ready(first));
        assert!(scope_alias.is_ready());
        independent.notify();
        assert!(scope.is_ready());
        assert!(scope.is_current(first));

        alias.notify();
        assert!(!scope_alias.is_ready());
        assert!(scope.is_directly_invalidated(first));
        assert!(!scope.set_ready(first));
        let pending = scope.generation();
        source.notify();
        assert!(!scope.is_current(pending));
        assert!(scope.is_directly_invalidated(pending));
        assert!(!scope.set_ready(pending));
        assert!(scope.set_ready(scope.generation()));
        assert!(!scope.set_ready(first));
        assert!(
            scope.is_ready(),
            "stale completion must not erase newer success"
        );

        drop(alias);
        drop(scope_alias);
        source.notify();
        assert!(!scope.is_ready(), "subscriptions outlive successful checks");
    }

    #[test]
    fn dependency_fanout_preserves_siblings() {
        let source = ReadyNotifier::new();
        let leaf = ReadyScope::new();
        let sibling = ReadyScope::new();
        let left = ReadyScope::new();
        let right = ReadyScope::new();
        let root = ReadyScope::new();
        leaf.subscribe(&source);
        assert!(leaf.set_ready(leaf.generation()));
        left.depend_on(&leaf);
        left.depend_on(&leaf);
        assert!(leaf.is_ready(), "register dependencies on cache hits too");
        left.depend_on(&sibling);
        right.depend_on(&leaf);
        root.depend_on(&left);
        root.depend_on(&right);
        root.depend_on(&root);
        assert_eq!(leaf.0.parents.borrow().len(), 2);
        assert_eq!(left.0.children.borrow().len(), 2);

        for scope in [&sibling, &left, &right, &root] {
            assert!(scope.set_ready(scope.generation()));
        }
        let snapshots = [&leaf, &left, &right, &root].map(|scope| (scope, scope.generation()));
        let sibling_before = sibling.generation();
        source.notify();
        for (scope, before) in snapshots {
            assert!(!scope.is_ready());
            assert_eq!(scope.generation().value, before.value + 1);
            assert!(!scope.set_ready(before));
            assert_eq!(
                scope.is_directly_invalidated(before),
                Rc::ptr_eq(&scope.0, &leaf.0)
            );
        }
        assert!(sibling.is_ready());
        assert!(sibling.is_current(sibling_before));

        // A directly subscribed ancestor must not be treated as propagation only.
        root.subscribe(&source);
        let pending = root.generation();
        let left_pending = left.generation();
        source.notify();
        assert!(root.is_directly_invalidated(pending));
        assert_eq!(root.generation().value, pending.value + 1);
        assert!(!left.is_directly_invalidated(left_pending));
        assert!(sibling.is_ready());
    }

    #[test]
    fn all_invalidation_precedes_reentrant_wakes() {
        let source = ReadyNotifier::new();
        let leaf = ReadyScope::new();
        let other = ReadyScope::new();
        let root = ReadyScope::new();
        leaf.subscribe(&source);
        other.subscribe(&source);
        root.depend_on(&leaf);
        root.depend_on(&other);
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let leaf_observer = leaf.observer();
        let other_observer = other.observer();
        let root_observer = root.observer();
        for observer in [&leaf_observer, &other_observer, &root_observer] {
            assert!(observer.register(&waker));
        }
        for scope in [&leaf, &other, &root] {
            assert!(scope.set_ready(scope.generation()));
        }

        let ran = Rc::new(Cell::new(false));
        let hook_ran = ran.clone();
        let hook_source = source.clone();
        let hook_leaf = leaf.clone();
        let hook_other = other.clone();
        let hook_root = root.clone();
        ON_WAKE.with_borrow_mut(|slot| {
            *slot = Some(Box::new(move || {
                for scope in [&hook_leaf, &hook_other, &hook_root] {
                    assert!(!scope.is_ready());
                    assert_eq!(scope.generation().value, 1);
                    assert!(
                        scope
                            .0
                            .observers
                            .0
                            .lock()
                            .unwrap()
                            .iter()
                            .all(|(_, w)| w.is_none())
                    );
                }
                let before = hook_root.generation();
                assert!(root_observer.register(&waker));
                hook_leaf.subscribe(&hook_source);
                hook_root.depend_on(&hook_other);
                drop(other_observer);
                hook_source.notify();
                assert!(!hook_root.is_current(before));
                assert!(!hook_root.is_directly_invalidated(before));
                assert!(!hook_root.set_ready(before));
                hook_ran.set(true);
            }));
        });
        source.notify();
        assert!(ran.get());
        assert_eq!(counter.wakes(), 4);
        for scope in [&leaf, &other, &root] {
            assert_eq!(scope.generation().value, 2);
        }
    }

    #[test]
    fn observers_update_coalesce_and_unregister() {
        let source = ReadyNotifier::new();
        let scope = ReadyScope::new();
        scope.subscribe(&source);
        let first = Arc::new(Counter::default());
        let latest = Arc::new(Counter::default());
        let first_waker = Waker::from(first.clone());
        let latest_waker = Waker::from(latest.clone());
        let observer = scope.observer();
        assert!(observer.register(&first_waker));
        let refs = Arc::strong_count(&first);
        assert!(observer.register(&first_waker));
        assert_eq!(Arc::strong_count(&first), refs);
        assert!(observer.register(&latest_waker));
        let cancelled = scope.observer();
        assert!(cancelled.register(&first_waker));
        drop(cancelled);
        source.notify();
        assert_eq!(first.wakes(), 0);
        assert_eq!(latest.wakes(), 1);
        let before = scope.generation();
        source.notify();
        assert_eq!(
            latest.wakes(),
            1,
            "wakes coalesce without losing invalidations"
        );
        assert!(!scope.is_current(before));

        assert!(observer.register(&latest_waker));
        source.notify();
        assert_eq!(latest.wakes(), 2);
        drop(observer);
        assert!(scope.0.observers.0.lock().unwrap().is_empty());
        assert!(scope.set_ready(scope.generation()));
        source.notify();
        assert!(
            !scope.is_ready(),
            "cancelling an observer must not unsubscribe"
        );
    }

    #[test]
    fn scheduling_wakes_do_not_invalidate() {
        let source = ReadyNotifier::new();
        let child = ReadyScope::new();
        let parent = ReadyScope::new();
        child.subscribe(&source);
        parent.depend_on(&child);
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let child_observer = child.observer();
        let parent_observer = parent.observer();
        assert!(child_observer.register(&waker));
        assert!(parent_observer.register(&waker));
        let before = child.generation();
        assert!(child.set_ready(before));
        assert!(parent.set_ready(parent.generation()));
        child.wake_observers();
        assert_eq!(counter.wakes(), 1);
        assert!(child.is_ready());
        assert!(child.is_current(before));
        assert!(parent.is_ready());
        source.notify();
        assert_eq!(counter.wakes(), 2);
        assert!(!child.is_ready());
        assert!(!parent.is_ready());
    }

    #[test]
    fn registration_cleanup_at_both_endpoints() {
        let source = ReadyNotifier::new();
        let parent = ReadyScope::new();
        let child = ReadyScope::new();
        parent.subscribe(&source);
        parent.depend_on(&child);
        let temporary_source = ReadyNotifier::new();
        let source_alias = temporary_source.clone();
        parent.subscribe(&temporary_source);
        drop(temporary_source);
        assert_eq!(parent.0.sources.borrow().len(), 2);
        drop(source_alias);
        assert_eq!(parent.0.sources.borrow().len(), 1);
        let temporary_child = ReadyScope::new();
        parent.depend_on(&temporary_child);
        drop(temporary_child);
        assert_eq!(parent.0.children.borrow().len(), 1);
        let temporary_parent = ReadyScope::new();
        temporary_parent.depend_on(&child);
        drop(temporary_parent);
        assert_eq!(child.0.parents.borrow().len(), 1);

        let observer = parent.observer();
        let checker_waker = parent.checker_waker();
        let observers = Arc::downgrade(&parent.0.observers);
        assert!(observer.register(&checker_waker));
        let weak = Rc::downgrade(&parent.0);
        let alias = parent.clone();
        drop(parent);
        assert!(weak.upgrade().is_some());
        assert_eq!(source.0.scopes.borrow().len(), 1);
        drop(alias);
        assert!(weak.upgrade().is_none());
        assert!(observers.upgrade().is_none());
        checker_waker.wake_by_ref();
        assert!(source.0.scopes.borrow().is_empty());
        assert!(child.0.parents.borrow().is_empty());
        assert!(!observer.register(Waker::noop()));
        drop(observer);
        source.notify();
    }

    #[test]
    fn weak_dependency_cycles_terminate_and_do_not_leak() {
        let source = ReadyNotifier::new();
        let a = ReadyScope::new();
        let b = ReadyScope::new();
        a.depend_on(&b);
        b.depend_on(&a);
        a.subscribe(&source);
        let before = a.generation();
        source.notify();
        assert_eq!(a.generation().value, before.value + 1);
        assert_eq!(b.generation().value, before.value + 1);
        let weak_a = Rc::downgrade(&a.0);
        let weak_b = Rc::downgrade(&b.0);
        drop(a);
        assert!(weak_a.upgrade().is_none());
        assert!(b.0.parents.borrow().is_empty());
        assert!(b.0.children.borrow().is_empty());
        drop(b);
        assert!(weak_b.upgrade().is_none());
        assert!(source.0.scopes.borrow().is_empty());
    }

    #[test]
    fn waker_drop_can_reenter_registration_and_notification() {
        let source = ReadyNotifier::new();
        let scope = ReadyScope::new();
        scope.subscribe(&source);
        let observer = scope.observer();
        assert!(observer.register(&Waker::from(Arc::new(DropHook))));
        let counter = Arc::new(Counter::default());
        let hook_source = source.clone();
        let hook_scope = scope.clone();
        ON_DROP.with_borrow_mut(|slot| {
            *slot = Some(Box::new(move || {
                assert_eq!(hook_scope.0.observers.0.lock().unwrap().len(), 1);
                hook_source.notify();
            }));
        });
        assert!(observer.register(&Waker::from(counter.clone())));
        assert_eq!(counter.wakes(), 1);

        assert!(observer.register(&Waker::from(Arc::new(DropHook))));
        let ran = Rc::new(Cell::new(false));
        let hook_ran = ran.clone();
        ON_DROP.with_borrow_mut(|slot| {
            *slot = Some(Box::new(move || {
                assert!(scope.0.observers.0.lock().unwrap().is_empty());
                let observer = scope.observer();
                assert!(observer.register(Waker::noop()));
                source.notify();
                hook_ran.set(true);
            }));
        });
        drop(observer);
        assert!(ran.get());
    }
}
