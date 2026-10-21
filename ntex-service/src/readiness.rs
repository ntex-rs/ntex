use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::HashSet,
    fmt,
    future::{Future, poll_fn},
    marker::PhantomData,
    pin::Pin,
    rc::{Rc, Weak},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, ThreadId},
};

/// A reusable callback invoked when a readiness scope receives a notification.
///
/// Register with [`crate::BindCtx::on_notify`] or [`crate::Pipeline::on_notify`].
/// The callback runs synchronously when [`ReadyNotifier::notify`] reaches the
/// registered scope, including notifications from dependencies and notifications
/// received while already invalid. It does not run for successful checks,
/// ordinary task wakeups, cached calls, or internal readiness resets.
///
/// Clones share one callback identity. Registering the same handle repeatedly on
/// a scope is idempotent; registering it on distinct affected scopes invokes it
/// once for each scope. Each scope retains its registration until it is dropped,
/// including across completed or cancelled checks.
///
/// Callbacks run after all affected scopes are invalidated, outside internal
/// borrows and locks, and before their observers are woken. Registrations made
/// during a notification apply to subsequent notifications. A nested
/// notification still invalidates every affected scope, but skips callback
/// delivery on scopes already executing callbacks. Registrations made on such
/// a scope are retained for notifications after its current delivery finishes.
/// Delivery reuses registered callback storage without cloning callback handles.
///
/// Captures must be owned. Avoid capturing a pipeline that retains this callback
/// or other strong references that would create a reference cycle.
/// This type is neither `Send` nor `Sync`.
#[derive(Clone)]
pub struct ReadyCallback(Rc<dyn Fn()>);

impl ReadyCallback {
    /// Creates an independent callback identity.
    pub fn new(callback: impl Fn() + 'static) -> Self {
        Self(Rc::new(callback))
    }
}

impl fmt::Debug for ReadyCallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyCallback").finish_non_exhaustive()
    }
}

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
/// Register sources with [`crate::BindCtx::subscribe`] in [`crate::Service::bind`].
#[derive(Clone, Debug, Default)]
pub struct ReadyNotifier(Rc<SourceInner>);

impl ReadyNotifier {
    /// Creates an independent notification source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Invalidates all subscribed readiness scopes and their ancestors, invokes
    /// their registered [`ReadyCallback`]s, then wakes their observers.
    ///
    /// Every notification advances the affected scopes' generations, including
    /// scopes that are already invalid or have a pending readiness check.
    /// Callback delivery is coalesced on scopes already executing callbacks.
    ///
    /// A callback panic propagates and skips remaining callbacks, but observers
    /// are still woken during unwinding. Wakers must not panic: a panicking
    /// waker skips the remaining wakers, and aborts if already unwinding.
    pub fn notify(&self) {
        let scopes = self.0.scopes.borrow();
        if let [scope] = scopes.as_slice() {
            let scope = scope.upgrade();
            drop(scopes);
            if let Some(scope) = scope {
                invalidate_scope(scope, true);
            }
        } else {
            let affected: Vec<_> = scopes
                .iter()
                .filter_map(Weak::upgrade)
                .map(|scope| (scope, 0))
                .collect();
            drop(scopes);
            invalidate_scopes(affected, true);
        }
    }
}

/// Invalidates one directly affected scope without allocating graph state when
/// it has no dependent ancestors.
fn invalidate_scope(scope: Rc<ScopeInner>, notify_callbacks: bool) {
    if !scope.parents.borrow().is_empty() {
        return invalidate_scopes(vec![(scope, 0)], notify_callbacks);
    }
    scope.invalidate(true);
    notify_scopes(&mut [(scope, 0)], notify_callbacks);
}

/// Affected graphs up to this size are deduplicated by linear scan.
const LINEAR_DEDUP: usize = 32;

fn invalidate_scopes(mut affected: Vec<(Rc<ScopeInner>, usize)>, notify_callbacks: bool) {
    let direct = affected.len();
    let mut visited: Option<HashSet<_, foldhash::fast::RandomState>> = None;

    let mut idx = 0;
    while idx < affected.len() {
        let scope = Rc::clone(&affected[idx].0);
        scope.invalidate(idx < direct);
        for parent in scope.parents.borrow().iter().filter_map(Weak::upgrade) {
            let ptr = Rc::as_ptr(&parent);
            let new = if let Some(visited) = visited.as_mut() {
                visited.insert(ptr)
            } else if affected.len() < LINEAR_DEDUP {
                !affected.iter().any(|(s, _)| Rc::as_ptr(s) == ptr)
            } else {
                let mut set: HashSet<_, _> = affected.iter().map(|(s, _)| Rc::as_ptr(s)).collect();
                let new = set.insert(ptr);
                visited = Some(set);
                new
            };
            if new {
                affected.push((parent, 0));
            }
        }
        idx += 1;
    }
    notify_scopes(&mut affected, notify_callbacks);
}

/// Runs callbacks of invalidated scopes, then wakes their observers, even if a
/// callback panics.
fn notify_scopes(affected: &mut [(Rc<ScopeInner>, usize)], notify_callbacks: bool) {
    // Freeze callback counts and drain wakers before user code can register again.
    let mut wakers = Wakers::default();
    for (scope, count) in &mut *affected {
        if notify_callbacks {
            *count = scope.callbacks.borrow().len();
        }
        scope.observers.take_wakers(&mut wakers, None);
    }
    for (scope, count) in &*affected {
        scope.notify_callbacks(*count);
    }
}

struct NotifyCallbacks<'a> {
    scope: &'a ScopeInner,
    callbacks: Vec<ReadyCallback>,
}

impl Drop for NotifyCallbacks<'_> {
    fn drop(&mut self) {
        let mut pending = self.scope.callbacks.borrow_mut();
        for callback in pending.drain(..) {
            if !self.callbacks.iter().any(|c| Rc::ptr_eq(&c.0, &callback.0)) {
                self.callbacks.push(callback);
            }
        }
        std::mem::swap(&mut *pending, &mut self.callbacks);
        self.scope.notifying.set(false);
    }
}

/// Wakers taken from observers, woken in order on drop.
///
/// Does not allocate for up to two wakers.
#[derive(Default)]
struct Wakers {
    inline: [Option<Waker>; 2],
    rest: Vec<Waker>,
}

impl Wakers {
    fn push(&mut self, waker: Waker) {
        if let Some(slot) = self.inline.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(waker);
        } else {
            self.rest.push(waker);
        }
    }
}

impl Drop for Wakers {
    fn drop(&mut self) {
        for slot in &mut self.inline {
            if let Some(waker) = slot.take() {
                waker.wake();
            }
        }
        for waker in self.rest.drain(..) {
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
        unlink(self.scopes.get_mut(), ptr, |scope| &scope.sources);
    }
}

/// Adds `item` unless already linked, returns whether it was added.
fn link<T>(links: &RefCell<Vec<Weak<T>>>, item: &Rc<T>) -> bool {
    let mut links = links.borrow_mut();
    let new = !contains(&links, item);
    if new {
        links.push(Rc::downgrade(item));
    }
    new
}

fn contains<T>(links: &[Weak<T>], item: &Rc<T>) -> bool {
    links.iter().any(|w| w.as_ptr() == Rc::as_ptr(item))
}

/// Drops the links, removing `ptr` from each live item's back-links.
fn unlink<T, U>(
    links: &mut Vec<Weak<T>>,
    ptr: *const U,
    back: impl Fn(&T) -> &RefCell<Vec<Weak<U>>>,
) {
    for item in links.drain(..).filter_map(|w| w.upgrade()) {
        back(&item).borrow_mut().retain(|w| w.as_ptr() != ptr);
    }
}

/// A snapshot for one scope's readiness check, not a transferable readiness permit.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Generation {
    value: u64,
    direct: u64,
}

impl From<Generation> for () {
    fn from(_: Generation) {}
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
        invalidate_scope(self.0.clone(), false);
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

    /// Subscribes during initialization of the service using this source.
    pub(crate) fn subscribe(&self, source: &ReadyNotifier) {
        if link(&self.0.sources, &source.0) {
            source.0.scopes.borrow_mut().push(Rc::downgrade(&self.0));
        }
    }

    pub(crate) fn on_notify(&self, callback: &ReadyCallback) {
        let mut callbacks = self.0.callbacks.borrow_mut();
        if !callbacks.iter().any(|c| Rc::ptr_eq(&c.0, &callback.0)) {
            callbacks.push(callback.clone());
        }
    }

    /// Registers a child during service binding.
    pub(crate) fn depend_on(&self, child: &Self) {
        if !Rc::ptr_eq(&self.0, &child.0) && link(&self.0.children, &child.0) {
            child.0.parents.borrow_mut().push(Rc::downgrade(&self.0));
        }
    }

    pub(crate) fn assert_dependency(&self, child: &Self) {
        assert!(
            Rc::ptr_eq(&self.0, &child.0) || contains(&self.0.children.borrow(), &child.0),
            "ServiceBinding must be registered by Service::bind before use"
        );
    }

    pub(crate) fn observer(&self) -> ReadyObserver {
        ReadyObserver {
            scope: Rc::downgrade(&self.0),
            idx: self.0.observers.slots.borrow_mut().insert(None),
        }
    }

    pub(crate) fn checker_waker(&self) -> Waker {
        self.0.observers.checker_waker()
    }

    /// Drives or joins a shared check without allocating on a cached success.
    pub(crate) async fn check<F, Fut, E>(&self, mut check: F) -> Result<(), E>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        // Reused across retries. Cancellation drops the check, then ownership,
        // then the observer.
        let mut observer = None;
        loop {
            if self.is_ready() {
                return Ok(());
            }

            let observer = observer.get_or_insert_with(|| self.observer());
            let owner = poll_fn(|cx| {
                observer.register(cx.waker());
                if self.is_ready() {
                    Poll::Ready(None)
                } else if self.0.checking.replace(true) {
                    Poll::Pending
                } else {
                    Poll::Ready(Some(CheckOwner(self)))
                }
            })
            .await;

            if let Some(owner) = owner {
                let result = RunCheck::new(self, &mut check).await.map(|generation| {
                    self.set_ready(generation);
                });
                drop(owner);
                result?;
            }
            // Cleanup and handoff can reenter and invalidate the success.
        }
    }

    /// Schedules observers without changing cached readiness or generations.
    pub(crate) fn wake_observers(&self) {
        self.0.observers.take_wakers(&mut Wakers::default(), None);
    }
}

#[derive(Debug, Default)]
struct ScopeInner {
    generation: Cell<Generation>,
    ready: Cell<bool>,
    checking: Cell<bool>,
    notifying: Cell<bool>,
    callbacks: RefCell<Vec<ReadyCallback>>,
    sources: RefCell<Vec<Weak<SourceInner>>>,
    parents: RefCell<Vec<Weak<Self>>>,
    children: RefCell<Vec<Weak<Self>>>,
    observers: Rc<ReadyObservers>,
}

thread_local! {
    static THREAD_ID: ThreadId = thread::current().id();
    static CHECKERS: RefCell<slab::Slab<Weak<ReadyObservers>>> = RefCell::default();
}

#[derive(Debug, Default)]
struct ReadyObservers {
    slots: RefCell<slab::Slab<Option<Waker>>>,
    checker: OnceCell<Arc<CheckerWake>>,
    _local: PhantomData<Rc<()>>,
}

impl ReadyObservers {
    fn checker_waker(self: &Rc<Self>) -> Waker {
        let checker = self.checker.get_or_init(|| {
            let idx = CHECKERS.with_borrow_mut(|slots| slots.insert(Rc::downgrade(self)));
            Arc::new(CheckerWake {
                owner: THREAD_ID.with(|id| *id),
                idx,
                closed: AtomicBool::new(false),
            })
        });
        Waker::from(checker.clone())
    }

    /// Takes registered wakers, except those with `skip` data.
    fn take_wakers(&self, wakers: &mut Wakers, skip: Option<*const ()>) {
        // Waker clone/drop/wake may reenter; keep them outside the borrow.
        for (_, waker) in self.slots.borrow_mut().iter_mut() {
            if let Some(waker) = waker.take_if(|waker| Some(waker.data()) != skip) {
                wakers.push(waker);
            }
        }
    }
}

impl Drop for ReadyObservers {
    fn drop(&mut self) {
        if let Some(checker) = self.checker.get() {
            // Invalidate old wakers before their registry slot can be reused.
            checker.closed.store(true, Ordering::Release);
            // The registry may already be destroyed during thread-local teardown.
            let _ = CHECKERS.try_with(|slots| slots.borrow_mut().remove(checker.idx));
        }
    }
}

// Only thread-safe routing metadata escapes the local observer storage.
#[derive(Debug)]
struct CheckerWake {
    owner: ThreadId,
    idx: usize,
    closed: AtomicBool,
}

impl Wake for CheckerWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        assert!(
            THREAD_ID.with(|id| *id == self.owner),
            "ntex-service readiness observers can only be accessed on their owning thread"
        );
        // Local destructors may wake a checker after the registry is destroyed.
        if let Ok(Some(observers)) =
            CHECKERS.try_with(|slots| slots.borrow().get(self.idx).and_then(Weak::upgrade))
        {
            // Checks polled by this waker register it too. Waking it would only
            // repeat this pass, so it stays registered for later notifications.
            let this = Arc::as_ptr(self).cast::<()>();
            observers.take_wakers(&mut Wakers::default(), Some(this));
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
    fn notify_callbacks(&self, count: usize) {
        if count == 0 || self.notifying.replace(true) {
            return;
        }
        // Moving the existing storage out leaves room for reentrant registrations.
        let callbacks = NotifyCallbacks {
            scope: self,
            callbacks: self.callbacks.take(),
        };
        for callback in callbacks.callbacks.iter().take(count) {
            (callback.0)();
        }
    }

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
}

impl Drop for ScopeInner {
    fn drop(&mut self) {
        let ptr = std::ptr::from_ref(self);
        unlink(self.sources.get_mut(), ptr, |source| &source.scopes);
        unlink(self.children.get_mut(), ptr, |child| &child.parents);
        unlink(self.parents.get_mut(), ptr, |parent| &parent.children);
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
            let waker = scope.observers.slots.borrow_mut()[self.idx].take();
            drop(waker);
        }
    }

    /// Registers before checking readiness; returns false if the scope is gone.
    pub(crate) fn register(&self, waker: &Waker) -> bool {
        let Some(scope) = self.scope.upgrade() else {
            return false;
        };
        if scope.observers.slots.borrow_mut()[self.idx]
            .as_ref()
            .is_some_and(|prev| prev.will_wake(waker))
        {
            return true;
        }

        // Waker clone/drop implementations can reenter, just like wake.
        let waker = waker.clone();
        let previous = scope.observers.slots.borrow_mut()[self.idx].replace(waker);
        drop(previous);
        true
    }
}

impl Drop for ReadyObserver {
    fn drop(&mut self) {
        if let Some(scope) = self.scope.upgrade() {
            let waker = scope.observers.slots.borrow_mut().remove(self.idx);
            drop(waker);
        }
    }
}

/// Runs a check until it completes without being invalidated.
///
/// Hand-written instead of an `async fn` to keep nested service readiness
/// futures shallow, deep `async` nesting overflows the compiler's query depth.
pub(crate) struct RunCheck<'a, F, Fut> {
    scope: &'a ReadyScope,
    check: F,
    // Dropped before `observer`.
    fut: Option<Fut>,
    observer: Option<ReadyObserver>,
    generation: Generation,
}

impl<'a, F, Fut, E> RunCheck<'a, F, Fut>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    pub(crate) fn new(scope: &'a ReadyScope, check: F) -> Self {
        Self {
            scope,
            check,
            fut: None,
            observer: None,
            generation: Generation::default(),
        }
    }

    pub(crate) fn scope(&self) -> &'a ReadyScope {
        self.scope
    }
}

impl<F, Fut, E> Future for RunCheck<'_, F, Fut>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    type Output = Result<Generation, E>;

    /// Polls the check. After completion, the next poll starts a new check.
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `fut` is pinned in place and only dropped in place.
        let this = unsafe { self.get_unchecked_mut() };
        let mut restarted = false;
        loop {
            // `&Option<Fut>` would retag the whole future and invalidate
            // borrows it holds into itself, `&mut` to a `!Unpin` future is not retagged.
            if this.fut.as_mut().is_none() {
                this.generation = this.scope.generation();
                this.fut = Some((this.check)());
            }
            if let Some(observer) = &this.observer {
                observer.register(cx.waker());
            }
            let result = if this.scope.is_directly_invalidated(this.generation) {
                None
            } else {
                let before_poll = this.scope.generation();
                // SAFETY: see above.
                let fut = unsafe { Pin::new_unchecked(this.fut.as_mut().unwrap()) };
                match fut.poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => {
                        let scope = this.scope;
                        this.observer
                            .get_or_insert_with(|| scope.observer())
                            .register(cx.waker());
                        // A first-poll notification may precede registration.
                        if !this.scope.is_current(before_poll) {
                            cx.waker().wake_by_ref();
                        }
                        return Poll::Pending;
                    }
                }
            };
            this.fut = None;
            if let Some(result) = result {
                this.observer = None;
                result?;
                if this.scope.is_current(this.generation) {
                    return Poll::Ready(Ok(this.generation));
                }
            }
            // A check that keeps invalidating its own scope must not starve the executor.
            if restarted {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            restarted = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        pin::pin,
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

        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.notify())).is_err()
        );
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
                Some(
                    "ntex-service readiness observers can only be accessed on their owning thread"
                )
            );
            drop(borrowed);
            assert_eq!(counter.wakes(), before);
            checker.wake_by_ref();
            assert_eq!(counter.wakes(), before + 1);
        }

        drop(scope);
        thread::spawn(move || checker.wake()).join().unwrap();
    }

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
}
