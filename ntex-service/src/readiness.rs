use std::cell::{Cell, OnceCell, RefCell};
use std::future::{Future, poll_fn};
use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, ThreadId};
use std::{collections::HashSet, fmt, marker::PhantomData, pin::Pin, rc::Rc, rc::Weak};

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
#[path = "../tests/unit/readiness.rs"]
mod tests;
