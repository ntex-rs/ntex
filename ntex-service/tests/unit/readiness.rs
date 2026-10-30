use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::{pin::pin, task::Context, task::Wake};

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

/// STATEMENT: Notification callbacks belong to scopes and run once per affected scope per notification.
///
/// HOW:
/// - Register aliased callbacks in a diamond dependency graph and an unaffected sibling.
/// - Verify delivery counts, no callback cloning, and callback release when all scopes are dropped.
#[test]
fn callbacks_follow_notifications_and_belong_to_scopes() {
    let source = ReadyNotifier::new();
    let leaf = ReadyScope::new();
    let left = ReadyScope::new();
    let right = ReadyScope::new();
    let root = ReadyScope::new();
    let sibling = ReadyScope::new();
    leaf.subscribe(&source);
    root.subscribe(&source);
    left.depend_on(&leaf);
    right.depend_on(&leaf);
    root.depend_on(&left);
    root.depend_on(&right);
    left.depend_on(&sibling);

    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    let identity = Rc::new(std::cell::OnceCell::<Weak<dyn Fn()>>::new());
    let captured_identity = identity.clone();
    let references = Rc::new(Cell::new(0));
    let captured_references = references.clone();
    let callback = ReadyCallback::new(move || {
        assert_eq!(
            captured_identity.get().unwrap().strong_count(),
            captured_references.get(),
            "delivery must not clone registered callbacks"
        );
        captured.set(captured.get() + 1);
    });
    identity.set(Rc::downgrade(&callback.0)).unwrap();
    for scope in [&leaf, &left, &right, &root, &sibling] {
        scope.on_notify(&callback);
        scope.on_notify(&callback.clone());
        assert!(scope.set_ready(scope.generation()));
    }
    drop(callback);
    references.set(identity.get().unwrap().strong_count());
    leaf.wake_observers();
    leaf.checker_waker().wake();
    leaf.reset_root();
    ReadyNotifier::new().notify();
    assert_eq!(calls.get(), 0);

    source.notify();
    assert_eq!(
        calls.get(),
        4,
        "once per affected scope, not per graph path"
    );
    source.notify();
    assert_eq!(calls.get(), 8, "notify again even while already invalid");
    assert!(sibling.is_ready());

    let weak_calls = Rc::downgrade(&calls);
    drop(calls);
    assert!(weak_calls.upgrade().is_some());
    drop((leaf, left, right, root, sibling));
    assert!(weak_calls.upgrade().is_none());
    source.notify();
}

/// STATEMENT: A notification snapshots callbacks and wakers before reentrant registration.
///
/// HOW:
/// - Register new callbacks and an observer while a notification callback is running.
/// - Verify new registrations wait for the next notification and repeated aliases stay deduplicated.
#[test]
fn notification_freezes_callback_counts_and_wakers_before_reentry() {
    let source = ReadyNotifier::new();
    let leaf = ReadyScope::new();
    let root = ReadyScope::new();
    leaf.subscribe(&source);
    root.depend_on(&leaf);
    let scopes = [Rc::downgrade(&leaf.0), Rc::downgrade(&root.0)];
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let leaf_observer = leaf.observer();
    let root_observer = root.observer();
    assert!(leaf_observer.register(&waker));
    assert!(root_observer.register(&waker));

    let late_calls = Rc::new(Cell::new(0));
    let captured = late_calls.clone();
    let late = ReadyCallback::new(move || captured.set(captured.get() + 1));
    let captured_source = source.clone();
    leaf.on_notify(&ReadyCallback::new(move || {
        let [leaf, root] = scopes.each_ref().map(|s| ReadyScope(s.upgrade().unwrap()));
        for scope in [&leaf, &root] {
            assert!(!scope.is_ready());
            assert!(
                scope
                    .0
                    .observers
                    .slots
                    .borrow_mut()
                    .iter()
                    .all(|(_, w)| w.is_none())
            );
        }
        leaf.on_notify(&late);
        leaf.on_notify(&late);
        root.on_notify(&late);
        leaf.subscribe(&captured_source);
        root.depend_on(&leaf);
        assert!(root_observer.register(&waker));
    }));
    for scope in [&leaf, &root] {
        assert!(scope.set_ready(scope.generation()));
    }

    source.notify();
    assert_eq!(
        late_calls.get(),
        0,
        "new registrations wait until next notify"
    );
    assert_eq!(counter.wakes(), 2, "do not drain reentrant registrations");
    source.notify();
    assert_eq!(late_calls.get(), 2);
    assert_eq!(counter.wakes(), 3);
    source.notify();
    assert_eq!(late_calls.get(), 4, "queued aliases remain idempotent");
    assert_eq!(counter.wakes(), 4);
}

/// STATEMENT: Reentrant notifications coalesce active callback delivery without losing invalidation.
///
/// HOW:
/// - Notify recursively from a callback after marking its scope ready again.
/// - Check generation advances, stale completion rejection, and delivery to an inactive ancestor.
#[test]
fn callbacks_coalesce_reentry_without_losing_invalidation() {
    let source = ReadyNotifier::new();
    let scope = ReadyScope::new();
    let root = ReadyScope::new();
    root.depend_on(&scope);
    scope.subscribe(&source);
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    let nested = source.clone();
    let weak = Rc::downgrade(&scope.0);
    let in_callback = Cell::new(false);
    scope.on_notify(&ReadyCallback::new(move || {
        assert!(!in_callback.replace(true), "delivery must be coalesced");
        captured.set(captured.get() + 1);
        let scope = ReadyScope(weak.upgrade().unwrap());
        let before = scope.generation();
        assert!(scope.set_ready(before));
        nested.notify();
        assert!(!scope.is_ready());
        assert!(
            !scope.set_ready(before),
            "nested invalidation must advance generation"
        );
        in_callback.set(false);
    }));
    let root_calls = Rc::new(Cell::new(0));
    let captured = root_calls.clone();
    root.on_notify(&ReadyCallback::new(move || {
        captured.set(captured.get() + 1);
    }));
    let before = scope.generation();
    source.notify();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        root_calls.get(),
        2,
        "inactive scopes still receive nested notifications"
    );
    assert_eq!(scope.generation().value, before.value + 2);
    source.notify();
    assert_eq!(calls.get(), 2, "delivery resumes after the previous batch");
    assert_eq!(root_calls.get(), 4);
    assert_eq!(scope.generation().value, before.value + 4);
}

/// STATEMENT: A callback panic still wakes invalidated observers and retains reentrant registrations.
///
/// HOW:
/// - Register a callback that adds another callback and panics during notification.
/// - Catch the panic, check all observers woke, then disable the panic and verify later delivery.
#[test]
fn callback_panic_still_wakes_invalidated_observers() {
    let source = ReadyNotifier::new();
    let scope = ReadyScope::new();
    scope.subscribe(&source);
    let panicking = Rc::new(Cell::new(true));
    let captured = panicking.clone();
    let late_calls = Rc::new(Cell::new(0));
    let captured_late = late_calls.clone();
    let late = ReadyCallback::new(move || captured_late.set(captured_late.get() + 1));
    let weak = Rc::downgrade(&scope.0);
    scope.on_notify(&ReadyCallback::new(move || {
        ReadyScope(weak.upgrade().unwrap()).on_notify(&late);
        assert!(!captured.get(), "callback panic");
    }));
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    scope.on_notify(&ReadyCallback::new(move || {
        captured.set(captured.get() + 1);
    }));
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let observers = [scope.observer(), scope.observer(), scope.observer()];
    for observer in &observers {
        assert!(observer.register(&waker));
    }
    assert!(scope.set_ready(scope.generation()));

    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.notify())).is_err());
    assert!(!scope.is_ready());
    assert_eq!(counter.wakes(), 3);
    assert_eq!(calls.get(), 0);
    assert_eq!(late_calls.get(), 0);
    panicking.set(false);
    for observer in &observers {
        assert!(observer.register(&waker));
    }
    source.notify();
    assert_eq!(counter.wakes(), 6);
    assert_eq!(calls.get(), 1);
    assert_eq!(
        late_calls.get(),
        1,
        "panic cleanup retains new registrations"
    );
}

/// STATEMENT: Notification during a check's first poll cannot be lost before observer registration.
///
/// HOW:
/// - Subscribe and notify from the first poll of a raw readiness check before returning Pending.
/// - Verify a wake is scheduled and the next poll returns the current generation without caching it.
#[test]
fn first_poll_notification_is_not_lost_before_observer_registration() {
    let source = ReadyNotifier::new();
    let scope = ReadyScope::new();
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let first_poll = Cell::new(true);
    let mut check = pin!(RunCheck::new(&scope, || {
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

/// STATEMENT: A readiness check that invalidates itself yields instead of spinning indefinitely.
///
/// HOW:
/// - Notify on every successful check attempt and poll the wrapper repeatedly.
/// - Verify each poll returns Pending after two attempts, schedules a wake, and leaves no valid cache.
#[test]
fn self_invalidating_check_yields() {
    let source = ReadyNotifier::new();
    let scope = ReadyScope::new();
    scope.subscribe(&source);
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let checks = Cell::new(0);
    let mut check = pin!(scope.check(|| {
        checks.set(checks.get() + 1);
        assert!(checks.get() < 100, "check never yields");
        source.notify();
        std::future::ready(Ok::<_, ()>(()))
    }));
    for poll in 1..=3 {
        assert!(
            check
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert_eq!(checks.get(), poll * 2);
        assert!(counter.wakes() >= poll);
    }
    assert!(!scope.is_ready());
}

/// STATEMENT: Invalidation triggered while handing off readiness forces a recheck before success.
///
/// HOW:
/// - Install an observer wake hook that invalidates the scope during completion.
/// - Verify the readiness future checks twice before returning a cached success.
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

/// STATEMENT: Subscriptions persist across successes and reject stale generations without erasing newer readiness.
///
/// HOW:
/// - Subscribe through aliased handles and verify registration deduplication.
/// - Notify repeatedly and check generation validity, stale completions, and persistence after alias drops.
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

/// STATEMENT: Dependency invalidation reaches shared ancestors once without invalidating unrelated siblings.
///
/// HOW:
/// - Build a diamond graph with duplicate and self dependencies plus a sibling.
/// - Notify the leaf, then directly subscribe the root, checking generations and direct-invalidation flags.
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

/// STATEMENT: Wide dependency fanout deduplicates shared ancestors during notification.
///
/// HOW:
/// - Build more leaves than the linear-dedup threshold with overlapping paths to two ancestors.
/// - Notify once and verify one root callback and one generation advance per ancestor.
#[test]
fn wide_fanout_invalidates_shared_ancestors_once() {
    let source = ReadyNotifier::new();
    let root = ReadyScope::new();
    let mid = ReadyScope::new();
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    root.on_notify(&ReadyCallback::new(move || {
        captured.set(captured.get() + 1);
    }));
    root.depend_on(&mid);
    let leaves: Vec<_> = (0..LINEAR_DEDUP * 2)
        .map(|idx| {
            let leaf = ReadyScope::new();
            leaf.subscribe(&source);
            // Reach `root` both directly and through `mid`.
            if idx % 2 == 0 { &root } else { &mid }.depend_on(&leaf);
            root.depend_on(&leaf);
            leaf
        })
        .collect();
    let root_before = root.generation();
    let mid_before = mid.generation();
    source.notify();
    assert_eq!(calls.get(), 1);
    assert_eq!(root.generation().value, root_before.value + 1);
    assert_eq!(mid.generation().value, mid_before.value + 1);
    assert!(!root.is_directly_invalidated(root_before));
    assert!(leaves.iter().all(|leaf| !leaf.is_ready()));
}

/// STATEMENT: All affected scopes are invalidated and their wakers extracted before any reentrant wake.
///
/// HOW:
/// - Wake an observer that inspects the whole graph, changes registrations, and notifies again.
/// - Verify no observer borrow remains held and both notification generations reach every scope.
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
                        .slots
                        .borrow_mut()
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

/// STATEMENT: Observers replace wakers, coalesce wakes, and unregister without removing subscriptions.
///
/// HOW:
/// - Re-register the same and a new waker, cancel another observer, and notify repeatedly.
/// - Check clone and wake counts, empty observer storage, and continuing cache invalidation.
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
    assert!(scope.0.observers.slots.borrow_mut().is_empty());
    assert!(scope.set_ready(scope.generation()));
    source.notify();
    assert!(
        !scope.is_ready(),
        "cancelling an observer must not unsubscribe"
    );
}

/// STATEMENT: Scheduling observer wakes does not invalidate readiness or propagate to parents.
///
/// HOW:
/// - Mark a child and parent ready and register observers on both.
/// - Wake child observers directly, then notify its source and compare cache and wake behavior.
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

/// STATEMENT: Readiness observer storage is neither Send nor Sync.
///
/// HOW:
/// - Use trait implementations that make type inference ambiguous for Send or Sync types.
/// - Require unambiguous inference for `ReadyObservers` at compile time.
#[test]
fn observer_storage_stays_local() {
    trait AmbiguousIfSendOrSync<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSendOrSync<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSendOrSync<u8> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSendOrSync<u16> for T {}

    // Inference becomes ambiguous if the storage gains Send or Sync.
    let _ = <ReadyObservers as AmbiguousIfSendOrSync<_>>::check;
}

/// STATEMENT: A stale checker waker cannot wake a new scope reusing its registry slot.
///
/// HOW:
/// - Drop a scope, create another in the same slot, and compare stale and current checker wakes.
/// - Verify registry cleanup and harmless remote wakes after both scopes are gone.
#[test]
fn stale_checker_cannot_wake_reused_registry_slot() {
    let before = CHECKERS.with_borrow(slab::Slab::len);
    let scope = ReadyScope::new();
    let stale = scope.checker_waker();
    let idx = scope.0.observers.checker.get().unwrap().idx;
    let storage = Rc::downgrade(&scope.0.observers);
    drop(scope);
    assert!(storage.upgrade().is_none());
    assert_eq!(CHECKERS.with_borrow(slab::Slab::len), before);

    let scope = ReadyScope::new();
    let checker = scope.checker_waker();
    assert_eq!(scope.0.observers.checker.get().unwrap().idx, idx);
    let observer = scope.observer();
    let counter = Arc::new(Counter::default());
    assert!(observer.register(&Waker::from(counter.clone())));
    stale.wake_by_ref();
    assert_eq!(counter.wakes(), 0);
    checker.wake_by_ref();
    assert_eq!(counter.wakes(), 1);
    drop(scope);
    assert_eq!(CHECKERS.with_borrow(slab::Slab::len), before);
    thread::spawn(move || {
        stale.wake();
        checker.wake();
    })
    .join()
    .unwrap();
}

/// STATEMENT: Checker wake delivery permits reentrant registry creation and destruction.
///
/// HOW:
/// - Install a wake hook that drops its owning scope and creates and drops another checker.
/// - Wake the original checker and verify delivery completes and the original storage is released.
#[test]
fn checker_wake_can_reenter_registry() {
    let scope = ReadyScope::new();
    let checker = scope.checker_waker();
    let storage = Rc::downgrade(&scope.0.observers);
    let observer = scope.observer();
    let counter = Arc::new(Counter::default());
    assert!(observer.register(&Waker::from(counter.clone())));
    ON_WAKE.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            drop(scope);
            let nested = ReadyScope::new();
            let checker = nested.checker_waker();
            drop(nested);
            checker.wake();
        }));
    });
    checker.wake();
    assert_eq!(counter.wakes(), 1);
    assert!(storage.upgrade().is_none());
}

/// STATEMENT: Checker wakers remain safe after owner-thread exit and during TLS registry teardown.
///
/// HOW:
/// - Keep a scope and wake-on-drop guard in TLS initialized before the checker registry.
/// - Exit the thread, then wake the checker retained by the joining thread.
#[test]
fn checker_outlives_owner_thread_and_registry_teardown() {
    struct WakeOnDrop(Waker);

    impl Drop for WakeOnDrop {
        fn drop(&mut self) {
            self.0.wake_by_ref();
        }
    }

    thread_local! {
        static LATE_SCOPE: RefCell<Option<(WakeOnDrop, ReadyScope)>> =
            const { RefCell::new(None) };
    }

    let checker = thread::spawn(|| {
        // Initialize this slot first so the registry is destroyed before it.
        LATE_SCOPE.with(|slot| {
            let scope = ReadyScope::new();
            let checker = scope.checker_waker();
            *slot.borrow_mut() = Some((WakeOnDrop(checker.clone()), scope));
            checker
        })
    })
    .join()
    .unwrap();
    checker.wake();
}

/// STATEMENT: Remote checker wakes reject live thread-local observer access before borrowing it.
///
/// HOW:
/// - Hold an observer-storage borrow while another thread calls `wake` and `wake_by_ref`.
/// - Check the owner-thread panic, successful later local delivery, and safe remote wake after teardown.
#[test]
fn checker_wakes_reject_remote_access_before_borrowing() {
    let scope = ReadyScope::new();
    let checker = scope.checker_waker();
    let observer = scope.observer();
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());

    for consume in [false, true] {
        assert!(observer.register(&waker));
        let before = counter.wakes();
        let borrowed = scope.0.observers.slots.borrow_mut();
        let remote = checker.clone();
        let error = thread::spawn(move || {
            if consume {
                remote.wake();
            } else {
                remote.wake_by_ref();
            }
        })
        .join()
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<&str>().copied(),
            Some("ntex-service readiness observers can only be accessed on their owning thread")
        );
        drop(borrowed);
        assert_eq!(counter.wakes(), before);
        checker.wake_by_ref();
        assert_eq!(counter.wakes(), before + 1);
    }

    drop(scope);
    thread::spawn(move || checker.wake()).join().unwrap();
}

/// STATEMENT: Remote checker cloning and dropping can race with local observer reuse and teardown.
///
/// HOW:
/// - Synchronize a worker's clone/drop loops with local observer registration, replacement, and slot reuse.
/// - Verify survivor wake counts, unchanged readiness, and storage release after scope teardown.
#[test]
fn checker_waker_clone_and_drop_race_local_observer_lifecycle() {
    let scope = ReadyScope::new();
    let before = scope.generation();
    assert!(scope.set_ready(before));
    let checker = scope.checker_waker();
    let storage = Rc::downgrade(&scope.0.observers);
    let iterations = if cfg!(miri) { 16 } else { 256 };

    std::thread::scope(|threads| {
        let (command, commands) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        let remote_checker = checker.clone();
        let worker = threads.spawn(move || {
            for _ in 0..=iterations {
                commands.recv().unwrap();
                for _ in 0..8 {
                    drop(remote_checker.clone());
                }
                commands.recv().unwrap();
                drop(remote_checker.clone());
                done.send(()).unwrap();
            }
            drop(remote_checker);
        });

        for _ in 0..iterations {
            let first = Arc::new(Counter::default());
            let latest = Arc::new(Counter::default());
            let survivor = Arc::new(Counter::default());
            command.send(()).unwrap();
            let observer = scope.observer();
            let idx = observer.idx;
            assert!(observer.register(&Waker::from(first.clone())));
            assert!(observer.register(&Waker::from(latest.clone())));
            observer.clear();
            drop(observer);
            let observer = scope.observer();
            assert_eq!(observer.idx, idx);
            assert!(observer.register(&Waker::from(survivor.clone())));
            command.send(()).unwrap();
            completed.recv().unwrap();
            checker.wake_by_ref();
            assert!(first.wakes() <= 1);
            assert!(latest.wakes() <= 1);
            assert_eq!(survivor.wakes(), 1);
            drop(observer);
            assert!(scope.is_current(before));
            assert!(scope.is_ready());
        }

        let observer = scope.observer();
        assert!(observer.register(&checker));
        command.send(()).unwrap();
        drop(scope);
        command.send(()).unwrap();
        completed.recv().unwrap();
        worker.join().unwrap();
        assert!(storage.upgrade().is_none());
        assert!(!observer.register(Waker::noop()));
        observer.clear();
        drop(observer);
    });
    checker.wake_by_ref();
}

/// STATEMENT: Dropping either endpoint removes notifier and dependency registrations without leaks.
///
/// HOW:
/// - Drop temporary sources, children, and parents while checking reciprocal registration counts.
/// - Drop the last scope alias and verify weak storage expires and stale observers become inert.
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
    let observers = Rc::downgrade(&parent.0.observers);
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

/// STATEMENT: Cyclic weak dependencies terminate notification and do not retain dropped scopes.
///
/// HOW:
/// - Connect two scopes cyclically, subscribe one, and verify one invalidation per scope.
/// - Drop each scope and check weak handles and reciprocal registrations are cleared.
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

/// STATEMENT: Replacing or dropping an observer waker permits reentrant registration and notification.
///
/// HOW:
/// - Use destructor hooks that inspect observer storage and notify while wakers are replaced.
/// - Drop an observer with another hook and verify it can register a new observer and notify safely.
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
            assert_eq!(hook_scope.0.observers.slots.borrow_mut().len(), 1);
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
            assert!(scope.0.observers.slots.borrow_mut().is_empty());
            let observer = scope.observer();
            assert!(observer.register(Waker::noop()));
            source.notify();
            hook_ran.set(true);
        }));
    });
    drop(observer);
    assert!(ran.get());
}
