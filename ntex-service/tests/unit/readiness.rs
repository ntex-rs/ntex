use std::rc::Weak;
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::{pin::pin, task::Context, task::Wake};

use super::*;

impl Endpoint {
    fn resolve(&self) -> Option<ReadyScope> {
        ReadyScope::resolve(&self.store, self.scope)
    }
}

impl ReadyScope {
    fn resolve(store: &Rc<WaitersInner>, id: usize) -> Option<Self> {
        store
            .scopes
            .with(id, |s| s.owners.set(s.owners.get() + 1))?;
        Some(Self(OnceCell::from(ScopeRef {
            store: store.clone(),
            id,
        })))
    }
}

thread_local! {
    static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
    static ON_DROP: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
    static STORE: crate::ctx::WaitersRef = crate::ctx::WaitersRef::new();
}

fn new_scope() -> ReadyScope {
    STORE.with(|store| {
        let store = store.scope().bound().store.clone();
        let id = store.scopes.allocate();
        ReadyScope(OnceCell::from(ScopeRef { store, id }))
    })
}

/// STATEMENT: Root scope zero and monotonic child IDs survive waiter moves and retire promptly.
///
/// HOW:
/// - Bind and retire children, verifying duplicate binding consumes no ID and retired IDs are not reused.
/// - Move the waiter owner, then drop it while keeping a child, observer, notifier, and checker waker.
/// - Verify callback and scheduling-waker release, an empty retired store, and inert retained notifier entries and handles.
#[test]
fn scope_ids_and_store_lifetime() {
    let waiters = crate::ctx::WaitersRef::new();
    assert_eq!(waiters.scope().bound().id, 0);
    let first = ReadyScope::new();
    assert!(first.0.get().is_none());
    waiters.scope().depend_on(&first);
    assert_eq!(first.bound().id, 1);
    waiters.scope().depend_on(&first);
    assert_eq!(first.bound().store.scopes.slots.borrow().len(), 2);
    drop(first);
    let child = ReadyScope::new();
    waiters.scope().depend_on(&child);
    assert_eq!(child.bound().id, 2);
    assert!(child.bound().store.scopes.slots.borrow()[1].is_none());

    let source = ReadyNotifier::new();
    child.subscribe(&source);
    let observer = child.observer();
    let checker = child.checker_waker();
    let counter = Rc::new(Cell::new(0));
    let weak = Rc::downgrade(&counter);
    child.on_notify(&ReadyCallback::new(move |_| counter.set(counter.get() + 1)));
    let endpoint = child.endpoint();
    let scheduled = Arc::new(Counter::default());
    waiters.get()[0] = Some(Waker::from(scheduled.clone()));
    let waiters = Box::new(waiters);
    source.notify();
    assert_eq!(weak.upgrade().unwrap().get(), 1);
    drop(waiters);
    assert_eq!(Arc::strong_count(&scheduled), 1);
    assert!(weak.upgrade().is_none());
    assert!(endpoint.resolve().is_none());
    assert!(child.bound().store.scopes.slots.borrow().is_empty());
    assert_eq!(source.0.borrow().as_slice(), [endpoint]);
    assert!(!observer.register(Waker::noop()));
    observer.clear();
    let remote = checker.clone();
    assert!(thread::spawn(move || remote.wake()).join().is_err());
    checker.wake();
    source.notify();
}

/// STATEMENT: Explicit links invalidate only the destination scope and ancestors, including internal resets.
///
/// HOW:
/// - Link a foreign root to a nested scope twice and keep an unrelated sibling ready.
/// - Verify the linked chain is invalidated before its callbacks and observer wakes.
/// - Verify successful source checks do not validate parents, resets suppress callbacks, and source drop makes saved links inert.
#[test]
fn explicit_links_preserve_scopes_and_reset_semantics() {
    let source_store = crate::ctx::WaitersRef::new();
    let parent_store = crate::ctx::WaitersRef::new();
    let parent = parent_store.scope();
    let destination = ReadyScope::new();
    let sibling = ReadyScope::new();
    parent.depend_on(&destination);
    parent.depend_on(&sibling);
    {
        let link = source_store.scope().readiness_link();
        destination.subscribe_pipeline(&link);
        destination.subscribe_pipeline(&link.clone());
    }
    assert_eq!(
        source_store
            .scope()
            .with(|s| s.external_parents.borrow().len()),
        1
    );

    let source = ReadyNotifier::new();
    source_store.scope().subscribe(&source);
    let affected = [source_store.scope(), &destination, parent];
    let endpoints = affected.map(ReadyScope::endpoint);
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    let captured_endpoints = endpoints.clone();
    let callback = ReadyCallback::new(move |_| {
        for endpoint in &captured_endpoints {
            assert!(!endpoint.resolve().unwrap().is_ready());
        }
        captured.set(captured.get() + 1);
    });
    for scope in affected {
        scope.on_notify(&callback);
        assert!(scope.set_ready(scope.generation()));
    }
    assert!(sibling.set_ready(sibling.generation()));
    let observer = destination.observer();
    let wakes = Arc::new(Counter::default());
    observer.register(&Waker::from(wakes.clone()));
    ON_WAKE.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            for endpoint in endpoints {
                assert!(!endpoint.resolve().unwrap().is_ready());
            }
        }));
    });
    source.notify();
    assert_eq!(calls.get(), 3);
    assert_eq!(wakes.wakes(), 1);
    assert!(sibling.is_ready());
    assert!(
        source_store
            .scope()
            .set_ready(source_store.scope().generation())
    );
    assert!(!destination.is_ready());
    assert!(!parent.is_ready());
    for scope in [&destination, parent] {
        assert!(scope.set_ready(scope.generation()));
    }
    source_store.scope().reset_root();
    assert_eq!(calls.get(), 3);
    assert!(!destination.is_ready());
    assert!(!parent.is_ready());
    assert!(sibling.is_ready());
    let stale_link = source_store.scope().readiness_link();
    drop(source_store);
    for scope in [&destination, parent] {
        assert!(scope.set_ready(scope.generation()));
    }
    destination.subscribe_pipeline(&stale_link);
    source.notify();
    assert!(destination.is_ready());
    assert!(parent.is_ready());
}

/// STATEMENT: Cross-store diamonds and cycles invalidate each scope once and release either endpoint.
///
/// HOW:
/// - Link three roots cyclically with two paths from the notified leaf to the top.
/// - Drop destinations, verifying their endpoints expire while notification skips retained stale parents.
/// - Create a replacement store and verify it is invalidated only after subscribing, then drop the source.
#[test]
fn explicit_link_cycles_tolerate_stale_endpoints() {
    let a = crate::ctx::WaitersRef::new();
    let b = crate::ctx::WaitersRef::new();
    let c = crate::ctx::WaitersRef::new();
    a.scope().subscribe_pipeline(&b.scope().readiness_link());
    a.scope().subscribe_pipeline(&c.scope().readiness_link());
    b.scope().subscribe_pipeline(&c.scope().readiness_link());
    c.scope().subscribe_pipeline(&a.scope().readiness_link());
    let source = ReadyNotifier::new();
    c.scope().subscribe(&source);
    source.notify();
    for scope in [a.scope(), b.scope(), c.scope()] {
        assert_eq!(scope.generation().value, 1);
    }
    let endpoint_b = b.scope().endpoint();
    drop(b);
    assert!(endpoint_b.resolve().is_none());
    assert_eq!(c.scope().with(|s| s.external_parents.borrow().len()), 2);
    source.notify();
    assert_eq!(a.scope().generation().value, 2);
    assert_eq!(c.scope().generation().value, 2);
    let endpoint_a = a.scope().endpoint();
    drop(a);
    assert!(endpoint_a.resolve().is_none());
    assert_eq!(c.scope().with(|s| s.external_parents.borrow().len()), 2);
    let replacement = crate::ctx::WaitersRef::new();
    assert!(
        replacement
            .scope()
            .set_ready(replacement.scope().generation())
    );
    source.notify();
    assert_eq!(c.scope().generation().value, 3);
    assert!(replacement.scope().is_ready());
    replacement
        .scope()
        .subscribe_pipeline(&c.scope().readiness_link());
    source.notify();
    assert_eq!(c.scope().generation().value, 4);
    assert_eq!(replacement.scope().generation().value, 1);
    assert!(!replacement.scope().is_ready());
    drop(c);
    assert_eq!(source.0.borrow().len(), 1);
    source.notify();
    assert_eq!(replacement.scope().generation().value, 1);
}

/// STATEMENT: Retired foreign destination scopes leave inert endpoints without affecting replacement scopes.
///
/// HOW:
/// - Subscribe a nested scope to a foreign pipeline, then retire it while its waiter store remains live.
/// - Keep a replacement scope and its parent ready through notification of the stale subscription.
/// - Subscribe the replacement twice and verify one new endpoint and invalidation of it and its parent.
#[test]
fn explicit_links_skip_retired_local_scopes() {
    let source_store = crate::ctx::WaitersRef::new();
    let parent_store = crate::ctx::WaitersRef::new();
    let destination = ReadyScope::new();
    parent_store.scope().depend_on(&destination);
    let link = source_store.scope().readiness_link();
    destination.subscribe_pipeline(&link);
    let endpoint = destination.endpoint();
    drop(destination);
    assert!(endpoint.resolve().is_none());
    assert!(
        source_store
            .scope()
            .with(|s| s.external_parents.borrow().as_slice() == [endpoint])
    );

    let replacement = ReadyScope::new();
    parent_store.scope().depend_on(&replacement);
    for scope in [&replacement, parent_store.scope()] {
        assert!(scope.set_ready(scope.generation()));
    }
    let source = ReadyNotifier::new();
    source_store.scope().subscribe(&source);
    source.notify();
    assert!(replacement.is_ready());
    assert!(parent_store.scope().is_ready());
    replacement.subscribe_pipeline(&link);
    replacement.subscribe_pipeline(&link);
    assert_eq!(
        source_store
            .scope()
            .with(|s| s.external_parents.borrow().len()),
        2
    );
    source.notify();
    assert!(!replacement.is_ready());
    assert!(!parent_store.scope().is_ready());
}

/// STATEMENT: Pipeline link variants share root zero and retain only the store allocation after retirement.
///
/// HOW:
/// - Compare fixed-state, state-taking, and attached bindings with their pipeline's link.
/// - Drop pipelines before their bindings and verify links expire only after the final runtime handle.
/// - Drop the remaining link and endpoint and verify the retired store allocation is released.
#[test]
fn pipeline_links_retain_only_store_allocations() {
    let fixed = crate::Pipeline::new((), crate::fn_service(async |(): ()| Ok::<_, ()>(())));
    let binding = fixed.bind();
    let link = fixed.readiness_link();
    assert!(Rc::ptr_eq(&link.store, &binding.readiness_link().store));
    let store = Rc::downgrade(&link.store);
    let endpoint = Endpoint {
        store: link.store.clone(),
        scope: 0,
    };
    drop(fixed);
    assert!(endpoint.resolve().is_some());
    drop(binding);
    assert!(endpoint.resolve().is_none());
    assert!(store.upgrade().unwrap().scopes.slots.borrow().is_empty());
    drop((link, endpoint));
    assert!(store.upgrade().is_none());

    let state =
        crate::pipeline::PipelineState::<(), _, _, _>::new(crate::fn_service(async |(): ()| {
            Ok::<_, ()>(())
        }));
    let binding = state.bind();
    let attached = state.bind_state(());
    let link = state.readiness_link();
    assert!(Rc::ptr_eq(&link.store, &binding.readiness_link().store));
    assert!(Rc::ptr_eq(&link.store, &attached.readiness_link().store));
    let endpoint = Endpoint {
        store: link.store,
        scope: 0,
    };
    drop((state, binding));
    assert!(endpoint.resolve().is_some());
    drop(attached);
    assert!(endpoint.resolve().is_none());
}

/// STATEMENT: Callback delivery rejects conflicting scope-table and callback-list mutations.
///
/// HOW:
/// - Attempt child binding or same-scope callback registration from a notification callback.
/// - Catch the borrow panic and verify neither storage changes nor scope ownership is lost.
/// - Notify again without the conflicting operation and verify delivery recovers.
#[test]
fn callbacks_reject_conflicting_storage_mutations() {
    for bind_child in [false, true] {
        let scope = new_scope();
        let source = ReadyNotifier::new();
        scope.subscribe(&source);
        let endpoint = scope.endpoint();
        let first = Cell::new(true);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        scope.on_notify(&ReadyCallback::new(move |_| {
            captured.set(captured.get() + 1);
            if first.replace(false) {
                let scope = endpoint.resolve().unwrap();
                if bind_child {
                    scope.depend_on(&ReadyScope::new());
                } else {
                    scope.on_notify(&ReadyCallback::new(|_| {}));
                }
            }
        }));
        let count = scope.bound().store.scopes.slots.borrow().len();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.notify())).is_err()
        );
        assert_eq!(scope.bound().store.scopes.slots.borrow().len(), count);
        assert_eq!(scope.with(|s| s.callbacks.borrow().len()), 1);
        assert_eq!(scope.with(|s| s.owners.get()), 1);
        source.notify();
        assert_eq!(calls.get(), 2);
    }
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
/// - Verify delivery counts, unchanged scope ownership, no callback cloning, and release after scope drop.
#[test]
fn callbacks_follow_notifications_and_belong_to_scopes() {
    let source = ReadyNotifier::new();
    let leaf = new_scope();
    let left = new_scope();
    let right = new_scope();
    let root = new_scope();
    let sibling = new_scope();
    leaf.subscribe(&source);
    root.subscribe(&source);
    left.depend_on(&leaf);
    right.depend_on(&leaf);
    root.depend_on(&left);
    root.depend_on(&right);
    left.depend_on(&sibling);

    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    let identity = Rc::new(std::cell::OnceCell::<Weak<dyn Fn(usize)>>::new());
    let captured_identity = identity.clone();
    let references = Rc::new(Cell::new(0));
    let captured_references = references.clone();
    let endpoints = [&leaf, &left, &right, &root, &sibling].map(ReadyScope::endpoint);
    let callback = ReadyCallback::new(move |_| {
        for endpoint in &endpoints {
            endpoint
                .store
                .scopes
                .with(endpoint.scope, |s| assert_eq!(s.owners.get(), 1));
        }
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

/// STATEMENT: Registrations on scopes not yet delivered can take effect in the current notification.
///
/// HOW:
/// - Register child and later-subscriber callbacks from an ancestor callback, plus a child observer waker.
/// - Subscribe the child twice and verify both deliveries see the new callback without duplicating its registration.
/// - Notify again and verify registrations and observer delivery remain usable.
#[test]
fn notification_delivers_later_registrations_without_snapshots() {
    let source = ReadyNotifier::new();
    let leaf = new_scope();
    let root = new_scope();
    let other = new_scope();
    leaf.subscribe(&source);
    leaf.subscribe(&source);
    other.subscribe(&source);
    root.depend_on(&leaf);
    let scopes = [leaf.endpoint(), other.endpoint()];
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let leaf_observer = leaf.observer();

    let late_calls = Rc::new(Cell::new(0));
    let captured = late_calls.clone();
    let late = ReadyCallback::new(move |_| captured.set(captured.get() + 1));
    root.on_notify(&ReadyCallback::new(move |_| {
        let [leaf, other] = scopes.each_ref().map(|s| s.resolve().unwrap());
        leaf.on_notify(&late);
        leaf.on_notify(&late);
        other.on_notify(&late);
        assert!(leaf_observer.register(&waker));
    }));
    for scope in [&leaf, &root, &other] {
        assert!(scope.set_ready(scope.generation()));
    }

    source.notify();
    assert_eq!(late_calls.get(), 3);
    assert_eq!(counter.wakes(), 1);
    source.notify();
    assert_eq!(late_calls.get(), 6);
    assert_eq!(counter.wakes(), 2);
}

/// STATEMENT: Nested notifications and resets are rejected even for another source and store.
///
/// HOW:
/// - Attempt a foreign source notification or readiness reset from a callback.
/// - Catch the borrow panic and verify the foreign scope's readiness and generation are unchanged.
/// - Deliver both sources afterward to verify the notification epoch guard recovers.
#[test]
fn nested_notifications_and_resets_panic() {
    for reset in [false, true] {
        let source = ReadyNotifier::new();
        let scope = new_scope();
        scope.subscribe(&source);
        let foreign = crate::ctx::WaitersRef::new();
        let nested = ReadyNotifier::new();
        foreign.scope().subscribe(&nested);
        let before = foreign.scope().generation();
        assert!(foreign.scope().set_ready(before));
        let endpoint = foreign.scope().endpoint();
        let captured_source = nested.clone();
        let first = Cell::new(true);
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        scope.on_notify(&ReadyCallback::new(move |_| {
            captured.set(captured.get() + 1);
            if first.replace(false) {
                if reset {
                    endpoint.resolve().unwrap().reset_root();
                } else {
                    captured_source.notify();
                }
            }
        }));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.notify())).is_err()
        );
        assert!(foreign.scope().is_ready());
        assert!(foreign.scope().is_current(before));
        source.notify();
        assert_eq!(calls.get(), 2);
        nested.notify();
        assert!(!foreign.scope().is_ready());
        assert!(!foreign.scope().is_current(before));
    }
}

/// STATEMENT: A callback panic wakes visited scopes while leaving later subscribers untouched.
///
/// HOW:
/// - Panic in an ancestor callback with observers on the ancestor, its child, and a later subscriber.
/// - Verify ancestor and child wakes, skipped callbacks, and unchanged readiness of the unvisited subscriber.
/// - Disable the panic and notify again, verifying complete delivery and wake recovery.
#[test]
fn callback_panic_still_wakes_invalidated_observers() {
    let source = ReadyNotifier::new();
    let scope = new_scope();
    let parent = new_scope();
    let other = new_scope();
    parent.depend_on(&scope);
    scope.subscribe(&source);
    other.subscribe(&source);
    let panicking = Rc::new(Cell::new(true));
    let captured = panicking.clone();
    parent.on_notify(&ReadyCallback::new(move |_| {
        assert!(!captured.get(), "callback panic");
    }));
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    let callback = ReadyCallback::new(move |_| {
        captured.set(captured.get() + 1);
    });
    scope.on_notify(&callback);
    parent.on_notify(&callback);
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let observers = [
        scope.observer(),
        parent.observer(),
        parent.observer(),
        parent.observer(),
        other.observer(),
    ];
    for observer in &observers {
        assert!(observer.register(&waker));
    }
    for scope in [&scope, &parent, &other] {
        assert!(scope.set_ready(scope.generation()));
    }
    let unvisited = other.generation();

    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.notify())).is_err());
    assert!(!scope.is_ready());
    assert!(!parent.is_ready());
    assert!(other.is_ready());
    assert!(other.is_current(unvisited));
    assert_eq!(counter.wakes(), 4);
    assert_eq!(calls.get(), 0);
    panicking.set(false);
    for observer in &observers {
        assert!(observer.register(&waker));
    }
    source.notify();
    assert_eq!(counter.wakes(), 9);
    assert_eq!(calls.get(), 2);
    assert!(!other.is_ready());
}

/// STATEMENT: Same-source notification and subscription reentry panic without retaining affected scopes.
///
/// HOW:
/// - Attempt each reentrant operation from a child callback after invalidating its parent.
/// - Catch the borrow panic, verify observer delivery and unchanged scope ownership, then notify successfully again.
/// - Retire the child and verify later notifications skip its stale endpoint.
#[test]
fn notifier_reentry_panics_without_retaining_scopes() {
    for subscribe in [false, true] {
        let source = ReadyNotifier::new();
        let scope = new_scope();
        let parent = new_scope();
        parent.depend_on(&scope);
        scope.subscribe(&source);
        let endpoint = scope.endpoint();
        let captured_endpoint = endpoint.clone();
        let reenter = Cell::new(true);
        let calls = Rc::new(Cell::new(0));
        let captured_calls = calls.clone();
        let captured_source = source.clone();
        scope.on_notify(&ReadyCallback::new(move |_| {
            captured_calls.set(captured_calls.get() + 1);
            if reenter.replace(false) {
                if subscribe {
                    captured_endpoint
                        .resolve()
                        .unwrap()
                        .subscribe(&captured_source);
                } else {
                    captured_source.notify();
                }
            }
        }));
        let observer = parent.observer();
        let counter = Arc::new(Counter::default());
        assert!(observer.register(&Waker::from(counter.clone())));
        assert!(parent.set_ready(parent.generation()));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.notify())).is_err()
        );
        assert!(!parent.is_ready());
        assert_eq!(counter.wakes(), 1);
        assert_eq!(scope.with(|s| s.owners.get()), 1);
        assert_eq!(source.0.borrow().len(), 1);
        source.notify();
        assert_eq!(calls.get(), 2);
        drop(scope);
        assert!(endpoint.resolve().is_none());
        source.notify();
        assert_eq!(calls.get(), 2);
    }
}

/// STATEMENT: Notification during a check's first poll cannot be lost before observer registration.
///
/// HOW:
/// - Subscribe and notify from the first poll of a raw readiness check before returning Pending.
/// - Verify a wake is scheduled and the next poll returns the current generation without caching it.
#[test]
fn first_poll_notification_is_not_lost_before_observer_registration() {
    let source = ReadyNotifier::new();
    let scope = new_scope();
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
    let scope = new_scope();
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
    let scope = new_scope();
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
/// - Subscribe through aliased handles and verify both registrations invalidate and invoke callbacks.
/// - Notify repeatedly and check generation validity, stale completions, and persistence after alias drops.
#[test]
fn persistent_subscriptions_and_generation_checks() {
    let source = ReadyNotifier::new();
    let alias = source.clone();
    let independent = ReadyNotifier::default();
    let scope = new_scope();
    let scope_alias = scope.clone();
    assert!(!scope.is_ready());
    scope.subscribe(&source);
    scope_alias.subscribe(&alias);
    assert_eq!(source.0.borrow().len(), 2);
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    scope.on_notify(&ReadyCallback::new(move |_| {
        captured.set(captured.get() + 1);
    }));

    let first = scope.generation();
    assert!(scope.set_ready(first));
    assert!(scope_alias.is_ready());
    independent.notify();
    assert!(scope.is_ready());
    assert!(scope.is_current(first));

    alias.notify();
    assert_eq!(scope.generation().value, first.value + 2);
    assert_eq!(calls.get(), 2);
    assert!(!scope_alias.is_ready());
    assert!(scope.is_directly_invalidated(first));
    assert!(!scope.set_ready(first));
    let pending = scope.generation();
    source.notify();
    assert_eq!(scope.generation().value, pending.value + 2);
    assert_eq!(calls.get(), 4);
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
    assert_eq!(calls.get(), 6);
}

/// STATEMENT: Dependency invalidation reaches shared ancestors once without invalidating unrelated siblings.
///
/// HOW:
/// - Build a diamond graph with duplicate and self dependencies plus a sibling.
/// - Notify the leaf, then directly subscribe the root, checking generations and direct-invalidation flags.
/// - Duplicate both direct subscriptions and verify they repeat without redelivering shared ancestors.
#[test]
fn dependency_fanout_preserves_siblings() {
    let source = ReadyNotifier::new();
    let leaf = new_scope();
    let sibling = new_scope();
    let left = new_scope();
    let right = new_scope();
    let root = new_scope();
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
    assert_eq!(leaf.with(|s| s.parents.borrow().len()), 2);
    left.assert_dependency(&leaf);
    left.assert_dependency(&sibling);

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
            scope.endpoint() == leaf.endpoint()
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

    leaf.subscribe(&source);
    root.subscribe(&source);
    let snapshots = [&leaf, &left, &right, &root].map(|scope| (scope, scope.generation()));
    source.notify();
    for (scope, before) in snapshots {
        let direct = scope.endpoint() == leaf.endpoint() || scope.endpoint() == root.endpoint();
        assert_eq!(
            scope.generation().value,
            before.value + if direct { 2 } else { 1 }
        );
        assert_eq!(scope.is_directly_invalidated(before), direct);
    }
}

/// STATEMENT: Wide dependency fanout deduplicates shared ancestors during notification.
///
/// HOW:
/// - Build 64 leaves with overlapping paths to two ancestors.
/// - Notify once and verify one root callback and one generation advance per ancestor.
#[test]
fn wide_fanout_invalidates_shared_ancestors_once() {
    let source = ReadyNotifier::new();
    let root = new_scope();
    let mid = new_scope();
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    root.on_notify(&ReadyCallback::new(move |_| {
        captured.set(captured.get() + 1);
    }));
    root.depend_on(&mid);
    let leaves: Vec<_> = (0..64)
        .map(|idx| {
            let leaf = new_scope();
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

/// STATEMENT: Notifications deliver each scope's callbacks and wakes before reaching later subscribers.
///
/// HOW:
/// - Subscribe two children sharing an ancestor and register callbacks and observers on all three.
/// - Inspect the first ancestor wake and verify the later child is still ready with its observer registered.
/// - Verify callback-before-wake ordering per scope and one invalidation per scope.
#[test]
fn notification_delivers_before_later_subscribers() {
    let source = ReadyNotifier::new();
    let leaf = new_scope();
    let other = new_scope();
    let root = new_scope();
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
    for (scope, wakes) in [(&root, 0), (&leaf, 1), (&other, 2)] {
        let captured = counter.clone();
        scope.on_notify(&ReadyCallback::new(move |_| {
            assert_eq!(captured.wakes(), wakes);
        }));
    }

    let ran = Rc::new(Cell::new(false));
    let hook_ran = ran.clone();
    let endpoints = [&leaf, &other, &root].map(ReadyScope::endpoint);
    ON_WAKE.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            let [leaf, other, root] = endpoints.each_ref().map(|e| e.resolve().unwrap());
            for scope in [&leaf, &root] {
                assert!(!scope.is_ready());
                assert_eq!(scope.generation().value, 1);
            }
            assert!(other.is_ready());
            assert_eq!(other.generation().value, 0);
            for scope in [&leaf, &other] {
                assert!(scope.with(|s| {
                    s.observers
                        .slots
                        .borrow_mut()
                        .iter()
                        .any(|(_, w)| w.is_some())
                }));
            }
            hook_ran.set(true);
        }));
    });
    source.notify();
    assert!(ran.get());
    assert_eq!(counter.wakes(), 3);
    for scope in [&leaf, &other, &root] {
        assert_eq!(scope.generation().value, 1);
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
    let scope = new_scope();
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
    assert!(scope.with(|s| s.observers.slots.borrow_mut().is_empty()));
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
    let child = new_scope();
    let parent = new_scope();
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

/// STATEMENT: Retired scope IDs are never reused and stale checker wakers cannot wake new scopes.
///
/// HOW:
/// - Drop a scope, create another in the same store, and verify its ID increased.
/// - Compare stale and current checker wakes and verify remote wakes are rejected even after retirement.
#[test]
fn stale_checker_cannot_wake_new_scope() {
    let scope = new_scope();
    let stale = scope.checker_waker();
    let endpoint = scope.endpoint();
    drop(scope);
    assert!(endpoint.resolve().is_none());

    let scope = new_scope();
    let checker = scope.checker_waker();
    assert!(Rc::ptr_eq(&scope.endpoint().store, &endpoint.store));
    assert!(scope.endpoint().scope > endpoint.scope);
    let observer = scope.observer();
    let counter = Arc::new(Counter::default());
    assert!(observer.register(&Waker::from(counter.clone())));
    stale.wake_by_ref();
    assert_eq!(counter.wakes(), 0);
    checker.wake_by_ref();
    assert_eq!(counter.wakes(), 1);
    drop(scope);
    assert!(!observer.register(Waker::noop()));
    for waker in [&stale, &checker] {
        let remote = waker.clone();
        assert!(thread::spawn(move || remote.wake()).join().is_err());
    }
}

/// STATEMENT: Checker wake delivery permits reentrant scope creation and destruction.
///
/// HOW:
/// - Install a wake hook that drops its owning scope and creates and drops another checker.
/// - Wake the original checker and verify delivery completes and the original storage is released.
#[test]
fn checker_wake_can_reenter_scope_storage() {
    let scope = new_scope();
    let checker = scope.checker_waker();
    let endpoint = scope.endpoint();
    let observer = scope.observer();
    let counter = Arc::new(Counter::default());
    assert!(observer.register(&Waker::from(counter.clone())));
    ON_WAKE.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            drop(scope);
            let nested = new_scope();
            let checker = nested.checker_waker();
            drop(nested);
            checker.wake();
        }));
    });
    checker.wake();
    assert_eq!(counter.wakes(), 1);
    assert!(endpoint.resolve().is_none());
}

/// STATEMENT: Checker wakers remain safe during owner-thread teardown after their store retires.
///
/// HOW:
/// - Keep a scope and wake-on-drop guard in TLS initialized before its waiter store.
/// - Exit the thread and verify the guard can wake and drop its retired checker locally.
#[test]
fn checker_wakes_during_owner_thread_teardown() {
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

    thread::spawn(|| {
        // Initialize this slot first so the waiter store is retired before it.
        LATE_SCOPE.with(|slot| {
            let scope = new_scope();
            let checker = scope.checker_waker();
            *slot.borrow_mut() = Some((WakeOnDrop(checker), scope));
        });
    })
    .join()
    .unwrap();
}

/// STATEMENT: Remote checker wakes reject live thread-local observer access before borrowing it.
///
/// HOW:
/// - Hold an observer-storage borrow while another thread calls `wake` and `wake_by_ref`.
/// - Check the owner-thread panic, successful later local delivery, and rejection of remote wake after teardown.
#[test]
fn checker_wakes_reject_remote_access_before_borrowing() {
    let scope = new_scope();
    let checker = scope.checker_waker();
    let observer = scope.observer();
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());

    for consume in [false, true] {
        assert!(observer.register(&waker));
        let before = counter.wakes();
        let slots = scope.bound().store.scopes.slots.borrow();
        let borrowed = slots[scope.bound().id]
            .as_ref()
            .unwrap()
            .observers
            .slots
            .borrow_mut();
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
        drop(slots);
        assert_eq!(counter.wakes(), before);
        checker.wake_by_ref();
        assert_eq!(counter.wakes(), before + 1);
    }

    drop(scope);
    let remote = checker.clone();
    assert!(thread::spawn(move || remote.wake()).join().is_err());
    checker.wake();
}

/// STATEMENT: Foreign final checker destruction panics without dropping its thread-local Rc, including while unwinding.
///
/// HOW:
/// - Retire a scope and move its last checker waker to another thread for drop or wake.
/// - Verify a single panic and an unchanged store reference count after foreign destruction.
/// - Reclaim the deliberately leaked reference on its owner thread to leave no test allocation behind.
#[test]
fn checker_final_drop_rejects_remote_rc_destruction() {
    for operation in 0..3 {
        let scope = new_scope();
        let store = scope.bound().store.clone();
        let checker = scope.checker_waker();
        drop(scope);
        let references = Rc::strong_count(&store);
        let error = thread::spawn(move || match operation {
            0 => drop(checker),
            1 => checker.wake(),
            _ => checker.wake_by_ref(),
        })
        .join()
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<&str>().copied(),
            Some("ntex-service readiness observers can only be accessed on their owning thread")
        );
        assert_eq!(Rc::strong_count(&store), references);
        // SAFETY: Foreign destruction leaked exactly the checker's Rc; reclaim
        // that strong reference on its owner while `store` keeps the allocation live.
        unsafe { Rc::decrement_strong_count(Rc::as_ptr(&store)) };
        assert_eq!(Rc::strong_count(&store), references - 1);
    }
}

/// STATEMENT: Remote checker cloning and dropping can race with local observer reuse and teardown.
///
/// HOW:
/// - Synchronize a worker's clone/drop loops with local observer registration, replacement, and slot reuse.
/// - Verify survivor wake counts, unchanged readiness, and storage release after scope teardown.
#[test]
fn checker_waker_clone_and_drop_race_local_observer_lifecycle() {
    let scope = new_scope();
    let before = scope.generation();
    assert!(scope.set_ready(before));
    let checker = scope.checker_waker();
    let endpoint = scope.endpoint();
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
        assert!(endpoint.resolve().is_none());
        assert!(!observer.register(Waker::noop()));
        observer.clear();
        drop(observer);
    });
    checker.wake_by_ref();
}

/// STATEMENT: Scope and notifier resources are released while stale parent IDs and notifier endpoints remain harmless.
///
/// HOW:
/// - Drop sources and scopes, checking notifier release, retired IDs, and retained parent IDs and endpoints.
/// - Drop the last scope alias and verify its ID is retired and stale observers become inert.
/// - Create a replacement parent and verify child notifications reach it only after explicit binding.
#[test]
fn registration_cleanup_tolerates_stale_parent_ids() {
    let source = ReadyNotifier::new();
    let parent = new_scope();
    let child = new_scope();
    parent.subscribe(&source);
    parent.depend_on(&child);
    let temporary_source = ReadyNotifier::new();
    let source_alias = temporary_source.clone();
    parent.subscribe(&temporary_source);
    let weak_source = Rc::downgrade(&source_alias.0);
    drop(temporary_source);
    assert!(weak_source.upgrade().is_some());
    drop(source_alias);
    assert!(weak_source.upgrade().is_none());
    let temporary_child = new_scope();
    parent.depend_on(&temporary_child);
    let temporary_endpoint = temporary_child.endpoint();
    drop(temporary_child);
    assert!(temporary_endpoint.resolve().is_none());
    let temporary_parent = new_scope();
    temporary_parent.depend_on(&child);
    drop(temporary_parent);
    assert_eq!(child.with(|s| s.parents.borrow().len()), 2);

    let observer = parent.observer();
    let checker_waker = parent.checker_waker();
    assert!(observer.register(&checker_waker));
    let endpoint = parent.endpoint();
    let alias = parent.clone();
    drop(parent);
    assert!(endpoint.resolve().is_some());
    assert_eq!(source.0.borrow().len(), 1);
    drop(alias);
    assert!(endpoint.resolve().is_none());
    checker_waker.wake_by_ref();
    assert_eq!(source.0.borrow().as_slice(), [endpoint]);
    assert_eq!(child.with(|s| s.parents.borrow().len()), 2);
    assert!(!observer.register(Waker::noop()));
    drop(observer);
    source.notify();

    let replacement = new_scope();
    let child_source = ReadyNotifier::new();
    child.subscribe(&child_source);
    assert!(replacement.set_ready(replacement.generation()));
    source.notify();
    assert!(replacement.is_ready());
    child_source.notify();
    assert!(replacement.is_ready());
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            replacement.assert_dependency(&child);
        }))
        .is_err()
    );
    replacement.depend_on(&child);
    replacement.assert_dependency(&child);
    child_source.notify();
    assert!(!replacement.is_ready());
}

/// STATEMENT: Cyclic ID dependencies terminate notification and do not retain dropped scopes.
///
/// HOW:
/// - Connect two scopes cyclically, subscribe one, and verify one invalidation per scope.
/// - Drop one scope and notify its survivor through a stale parent ID without resurrecting the retired scope.
/// - Drop the survivor and verify both endpoints expire while stale notifier entries remain safe to notify.
#[test]
fn dependency_cycles_terminate_and_do_not_leak() {
    let source = ReadyNotifier::new();
    let a = new_scope();
    let b = new_scope();
    a.depend_on(&b);
    b.depend_on(&a);
    a.subscribe(&source);
    let before = a.generation();
    source.notify();
    assert_eq!(a.generation().value, before.value + 1);
    assert_eq!(b.generation().value, before.value + 1);
    let endpoint_a = a.endpoint();
    let endpoint_b = b.endpoint();
    drop(a);
    assert!(endpoint_a.resolve().is_none());
    assert_eq!(b.with(|s| s.parents.borrow().len()), 1);
    b.subscribe(&source);
    source.notify();
    assert_eq!(b.generation().value, before.value + 2);
    drop(b);
    assert!(endpoint_b.resolve().is_none());
    assert_eq!(source.0.borrow().as_slice(), [endpoint_a, endpoint_b]);
    source.notify();
}

/// STATEMENT: Replacing or dropping an observer waker permits reentrant registration and notification.
///
/// HOW:
/// - Use destructor hooks that inspect observer storage and notify while wakers are replaced.
/// - Drop an observer with another hook and verify it can register a new observer and notify safely.
#[test]
fn waker_drop_can_reenter_registration_and_notification() {
    let source = ReadyNotifier::new();
    let scope = new_scope();
    scope.subscribe(&source);
    let observer = scope.observer();
    assert!(observer.register(&Waker::from(Arc::new(DropHook))));
    let counter = Arc::new(Counter::default());
    let hook_source = source.clone();
    let hook_scope = scope.clone();
    ON_DROP.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            assert_eq!(hook_scope.with(|s| s.observers.slots.borrow_mut().len()), 1);
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
            assert!(scope.with(|s| s.observers.slots.borrow_mut().is_empty()));
            let observer = scope.observer();
            assert!(observer.register(Waker::noop()));
            source.notify();
            hook_ran.set(true);
        }));
    });
    drop(observer);
    assert!(ran.get());
}
