use std::cell::{Cell, OnceCell, RefCell};
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::{fmt, marker::PhantomData, mem, pin::Pin, rc::Rc, thread, thread::ThreadId};

use crate::ctx::WaitersInner;

thread_local! {
    static NOTIFICATION: RefCell<u64> = const { RefCell::new(0) };
}

/// A reusable callback invoked when a readiness scope receives a notification.
///
/// Register with [`crate::BindCtx::on_notify`] or [`crate::Pipeline::on_notify`].
/// The callback runs synchronously when [`ReadyNotifier::notify`] reaches the
/// registered scope, including notifications from dependencies and notifications
/// received while already invalid. It does not run for successful checks,
/// ordinary task wakeups, cached calls, or internal readiness resets.
///
/// The argument is the receiving scope's user data, configured with
/// [`crate::ReadyBinding::with_data`], or `0` by default. Propagation delivers
/// each scope's own value, not the originating scope's value. Pipeline roots
/// always supply `0`. One callback can be shared by scopes with different data.
///
/// Clones share one callback identity. Registering the same handle repeatedly on
/// a scope is idempotent. Repeated notifier subscriptions can invoke it more than
/// once per notification. Each scope retains its registration until it is
/// dropped, including across completed or cancelled checks.
///
/// Callbacks run after their scope is invalidated and its parents are visited,
/// before its observers are woken. Later subscribers and unvisited ancestors
/// may still be ready. Delivery borrows scope and callback storage in place,
/// without snapshots or cloning callback handles. Registering callbacks on a
/// scope currently executing callbacks, modifying borrowed scope storage, or
/// notifying any source during delivery panics. Registrations on other scopes
/// can take effect in the current notification.
///
/// Captures must be owned. Avoid capturing a pipeline that retains this callback
/// or other strong references that would create a reference cycle.
/// This type is neither `Send` nor `Sync`.
#[derive(Clone)]
pub struct ReadyCallback(Rc<dyn Fn(usize)>);

impl ReadyCallback {
    /// Creates an independent callback identity.
    pub fn new(callback: impl Fn(usize) + 'static) -> Self {
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
/// their dependent ancestors as it visits them, delivering each scope's callbacks
/// and observer wakes before continuing to later subscribers. It requests another
/// readiness check; it does not grant readiness or run or drop readiness futures.
///
/// Clones share the same notification source. [`Default`] and [`new`](Self::new)
/// each create an independent source. Subscriptions belong to readiness scopes
/// and persist until the scope or source is dropped, even after a readiness
/// check completes or is cancelled. Repeated subscriptions are not deduplicated
/// and cause repeated invalidations and callback delivery. Entries for dropped
/// scopes are ignored and remain stored until the source is dropped. These
/// entries retain waiter-store allocations, but not retired scope resources.
///
/// Notify after any change that can affect readiness, including consuming the
/// last available slot and releasing capacity. The change and notification must
/// happen before another caller can use cached readiness.
///
/// This type is neither `Send` nor `Sync`.
/// Register sources with [`crate::BindCtx::subscribe`] in [`crate::Service::bind`].
#[derive(Clone, Debug, Default)]
pub struct ReadyNotifier(Rc<RefCell<Vec<Endpoint>>>);

impl ReadyNotifier {
    /// Creates an independent notification source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Visits subscribed scopes in registration order, invalidating each scope,
    /// recursively visiting its parents, then invoking its [`ReadyCallback`]s
    /// and waking its observers. Directly subscribed ancestors are deferred to
    /// their own subscription entries.
    ///
    /// Every notification advances the affected scopes' generations, including
    /// scopes that are already invalid or have a pending readiness check.
    /// Shared ancestors are visited once; duplicate direct subscriptions still
    /// invalidate and deliver callbacks repeatedly. Traversal uses stack space
    /// proportional to dependency depth, without allocating traversal buffers.
    ///
    /// A callback panic propagates, waking already-invalidated scopes during
    /// unwinding; unvisited scopes remain untouched. Wakers must not panic:
    /// a panicking waker skips its remaining siblings and can abort if another
    /// waker panics during unwinding.
    ///
    /// # Panics
    ///
    /// Panics if any readiness notification or internal readiness reset is
    /// already being delivered on this thread. Subscribing to this source or
    /// mutating borrowed scope or callback storage during delivery also panics.
    pub fn notify(&self) {
        with_notification(|epoch| {
            let scopes = self.0.borrow();
            // Direct subscribers must not also be delivered as ancestors.
            for endpoint in &*scopes {
                endpoint
                    .store
                    .scopes
                    .with(endpoint.scope, |s| s.direct_epoch.set(epoch));
            }
            for endpoint in &*scopes {
                notify_scope(&endpoint.store, endpoint.scope, epoch, true, true);
            }
        });
    }
}

/// Runs one notification or reset with a fresh thread-local traversal epoch.
///
/// Holds the epoch's mutable borrow until delivery finishes, including unwinding.
///
/// # Panics
///
/// Panics on nested delivery on this thread or when the epoch counter overflows.
fn with_notification(f: impl FnOnce(u64)) {
    NOTIFICATION.with_borrow_mut(|epoch| {
        *epoch = epoch
            .checked_add(1)
            .expect("readiness notification epoch exhausted");
        f(*epoch);
    });
}

/// Invalidates a live scope, visits its parents, then delivers callbacks and wakes.
///
/// Missing IDs are ignored. Indirect visits skip scopes already visited or marked
/// for direct delivery in this epoch; duplicate direct visits remain effective.
/// Setting `callbacks` to false suppresses callbacks throughout the traversal.
///
/// Scope and parent storage remain borrowed during recursion and delivery.
/// Already-invalidated scopes wake their observers even if a callback panics;
/// unvisited scopes remain untouched.
fn notify_scope(store: &WaitersInner, id: usize, epoch: u64, direct: bool, callbacks: bool) {
    store.scopes.with(id, |s| {
        if !direct && (s.notify_epoch.get() == epoch || s.direct_epoch.get() == epoch) {
            return;
        }
        s.notify_epoch.set(epoch);
        s.invalidate(direct);
        let _wake = NotifyObservers(&s.observers);
        for &parent in s.parents.borrow().iter() {
            notify_scope(store, parent, epoch, false, callbacks);
        }
        for parent in s.external_parents.borrow().iter() {
            notify_scope(&parent.store, parent.scope, epoch, false, callbacks);
        }
        if callbacks {
            for callback in s.callbacks.borrow().iter() {
                (callback.0)(s.data.get());
            }
        }
    });
}

struct NotifyObservers<'a>(&'a ReadyObservers);

impl Drop for NotifyObservers<'_> {
    /// Takes and wakes observer slots in place, skipping the scope's own checker.
    ///
    /// Uses the initial slab capacity as a bound rather than snapshotting wakers.
    /// Releases each slot borrow before waking; a waker panic stops this pass.
    fn drop(&mut self) {
        // The local checker only forwards to these same slots.
        let skip = self.0.checker.get().map(|c| Arc::as_ptr(c).cast());
        let capacity = self.0.slots.borrow().capacity();
        for idx in 0..capacity {
            let waker = self
                .0
                .slots
                .borrow_mut()
                .get_mut(idx)
                .and_then(Option::take);
            if let Some(waker) = waker
                && Some(waker.data()) != skip
            {
                waker.wake();
            }
        }
    }
}

/// Wakers taken from observers, woken in order on drop.
///
/// Does not allocate for up to eight wakers.
#[derive(Default)]
struct Wakers {
    inline: [Option<Waker>; 8],
    rest: Vec<Waker>,
}

impl Wakers {
    /// Queues a waker for delivery on drop, using inline storage for the first eight.
    fn push(&mut self, waker: Waker) {
        if let Some(slot) = self.inline.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(waker);
        } else {
            self.rest.push(waker);
        }
    }
}

impl Drop for Wakers {
    /// Wakes queued tasks in insertion order; a panic skips remaining delivery.
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

/// Appends a dependency edge if absent, returning whether it was inserted.
///
/// Panics if the edge list is already borrowed.
fn link<T: PartialEq>(links: &RefCell<Vec<T>>, item: T) -> bool {
    let mut links = links.borrow_mut();
    let new = !links.contains(&item);
    if new {
        links.push(item);
    }
    new
}

/// An explicit subscription endpoint for another pipeline's readiness.
///
/// Obtain a link with [`crate::Pipeline::readiness_link`] and register it with
/// [`crate::BindCtx::subscribe_pipeline`]. Invalidation of the source pipeline
/// invalidates the subscribing scope and its ancestors, including internal
/// resets. A link does not check readiness or retain the source pipeline.
/// The enclosing service must still await the source pipeline's readiness.
///
/// Registrations belong to the subscribing scope, not this handle. Dropping a
/// temporary link does not unsubscribe it. Links become inert when their source
/// pipeline is dropped. Links and registrations retain the waiter-store
/// allocation, but not retired scope resources. This type is neither `Send` nor
/// `Sync`.
#[derive(Clone)]
pub struct ReadyLink {
    store: Rc<WaitersInner>,
}

impl fmt::Debug for ReadyLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyLink")
            .field("store", &Rc::as_ptr(&self.store))
            .finish()
    }
}

#[derive(Clone)]
struct Endpoint {
    store: Rc<WaitersInner>,
    scope: usize,
}

impl PartialEq for Endpoint {
    /// Compares scope IDs within the same waiter-store allocation.
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope && Rc::ptr_eq(&self.store, &other.store)
    }
}

impl fmt::Debug for Endpoint {
    /// Formats the store address and scope ID without accessing scope storage.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Endpoint")
            .field("store", &Rc::as_ptr(&self.store))
            .field("scope", &self.scope)
            .finish()
    }
}

/// The append-only scope table owned by a pipeline's waiter store.
#[derive(Debug)]
pub(crate) struct ScopeStore {
    slots: RefCell<Vec<Option<ScopeInner>>>,
}

impl Default for ScopeStore {
    /// Creates root ID `0` with one owner reserved for the initial root handle.
    fn default() -> Self {
        Self {
            slots: RefCell::new(vec![Some(ScopeInner::default())]),
        }
    }
}

impl ScopeStore {
    /// Appends an unready scope with one owner and returns its never-reused ID.
    ///
    /// # Panics
    ///
    /// Panics if the store is retired or its scope table is already borrowed.
    fn allocate(&self) -> usize {
        let mut slots = self.slots.borrow_mut();
        assert!(!slots.is_empty(), "readiness store has been retired");
        let id = slots.len();
        slots.push(Some(ScopeInner::default()));
        id
    }

    /// Runs `f` with a live scope, returning `None` for missing or retired IDs.
    ///
    /// Keeps the table immutably borrowed throughout `f`; allocating or retiring
    /// scopes during the closure panics.
    fn with<R>(&self, id: usize, f: impl FnOnce(&ScopeInner) -> R) -> Option<R> {
        self.slots.borrow().get(id).and_then(Option::as_ref).map(f)
    }

    /// Releases one logical scope owner, ignoring already-retired IDs.
    ///
    /// The last child owner removes its slot. The last root owner retires the
    /// entire table, closing all checkers before dropping scope resources.
    /// Retained endpoints keep only the waiter-store allocation alive.
    ///
    /// # Panics
    ///
    /// Panics if retirement requires mutating a borrowed scope table.
    fn release(&self, id: usize) {
        if self.with(id, |s| {
            s.owners.set(s.owners.get() - 1);
            s.owners.get() == 0
        }) != Some(true)
        {
            return;
        }
        if id == 0 {
            let slots = self.slots.take();
            // Close every checker before dropping reentrant callback captures or wakers.
            for scope in slots.iter().flatten() {
                scope.observers.close();
            }
        } else {
            let scope = self.slots.borrow_mut()[id].take().unwrap();
            scope.observers.close();
        }
    }
}

/// A snapshot for one scope's readiness check, not a transferable readiness permit.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Generation {
    value: u64,
    direct: u64,
}

impl From<Generation> for () {
    /// Discards the snapshot when a caller only needs readiness completion.
    fn from(_: Generation) {}
}

/// A lazily bound local scope handle; all scope data lives in its waiter store.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReadyScope(OnceCell<ScopeRef>);

#[derive(Debug)]
struct ScopeRef {
    store: Rc<WaitersInner>,
    id: usize,
}

impl Clone for ScopeRef {
    /// Acquires another logical owner of this live scope and clones its store.
    ///
    /// Panics if the scope has been retired.
    fn clone(&self) -> Self {
        self.store
            .scopes
            .with(self.id, |s| s.owners.set(s.owners.get() + 1))
            .expect("readiness scope has been retired");
        Self {
            store: self.store.clone(),
            id: self.id,
        }
    }
}

impl Drop for ScopeRef {
    /// Releases logical ownership, retiring the scope on its last owner.
    fn drop(&mut self) {
        self.store.scopes.release(self.id);
    }
}

impl ReadyScope {
    /// Creates an unbound handle without allocating a scope or waiter store.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adopts the initial owner of the store's preallocated root ID `0`.
    ///
    /// Does not increment the owner count; use cloning for additional handles.
    pub(crate) fn root(store: Rc<WaitersInner>) -> Self {
        Self(OnceCell::from(ScopeRef { store, id: 0 }))
    }

    /// Returns the assigned store and ID without checking whether the scope is live.
    ///
    /// Panics if service binding has not assigned this handle to a store.
    fn bound(&self) -> &ScopeRef {
        self.0
            .get()
            .expect("ReadyBinding must be registered by Service::bind before use")
    }

    /// Accesses this scope while holding an immutable borrow of its store's table.
    ///
    /// Panics if the handle is unbound or the scope is retired. The closure must
    /// not allocate or retire scopes in the borrowed table.
    fn with<R>(&self, f: impl FnOnce(&ScopeInner) -> R) -> R {
        let scope = self.bound();
        scope
            .store
            .scopes
            .with(scope.id, f)
            .expect("readiness scope has been retired")
    }

    /// Returns an endpoint retaining the store allocation, not a logical scope owner.
    ///
    /// Panics if this handle is unbound; retired scopes can still produce endpoints.
    fn endpoint(&self) -> Endpoint {
        let scope = self.bound();
        Endpoint {
            store: scope.store.clone(),
            scope: scope.id,
        }
    }

    /// Returns an explicit subscription link for this pipeline root.
    ///
    /// Panics if the handle is unbound or its scope ID is not `0`.
    pub(crate) fn readiness_link(&self) -> ReadyLink {
        assert_eq!(self.bound().id, 0);
        ReadyLink {
            store: self.bound().store.clone(),
        }
    }

    /// Makes invalidation of the linked source root propagate to this scope.
    ///
    /// Deduplicates dependency edges, ignores root self-links, and does nothing
    /// for retired sources. Same-store edges use local IDs; foreign edges retain
    /// a destination endpoint without acquiring a logical scope owner.
    ///
    /// Panics if the source is live but this handle is unbound, or if registration
    /// conflicts with a borrow of the source's parent list.
    pub(crate) fn subscribe_pipeline(&self, source_link: &ReadyLink) {
        source_link.store.scopes.with(0, |source| {
            let scope = self.bound();
            if Rc::ptr_eq(&scope.store, &source_link.store) {
                if scope.id != 0 {
                    link(&source.parents, scope.id);
                }
            } else {
                link(&source.external_parents, self.endpoint());
            }
        });
    }

    /// Returns the live scope's validation and direct-invalidation snapshot.
    pub(crate) fn generation(&self) -> Generation {
        self.with(|s| s.generation.get())
    }

    /// Returns whether no invalidation has occurred since the supplied snapshot.
    pub(crate) fn is_current(&self, generation: Generation) -> bool {
        self.generation() == generation
    }

    /// Returns whether direct invalidation has occurred since the snapshot.
    ///
    /// Invalidation propagated from a child changes the validation generation,
    /// but only direct invalidation requires restarting an outstanding check.
    pub(crate) fn is_directly_invalidated(&self, generation: Generation) -> bool {
        self.generation().direct != generation.direct
    }

    /// Returns cached readiness, or false for an unbound or retired scope.
    pub(crate) fn is_ready(&self) -> bool {
        self.0.get().is_some_and(|scope| {
            scope
                .store
                .scopes
                .with(scope.id, |s| s.ready.get())
                .unwrap_or(false)
        })
    }

    /// Directly invalidates this scope and visits its parents without callbacks.
    ///
    /// Wakes affected observers without running or dropping readiness futures.
    /// Despite the name, this also supports nested scopes during service creation.
    ///
    /// Panics if this handle is unbound or another notification or reset is being
    /// delivered on this thread. Retired scopes are ignored.
    pub(crate) fn reset_root(&self) {
        let scope = self.bound();
        with_notification(|epoch| {
            scope
                .store
                .scopes
                .with(scope.id, |s| s.direct_epoch.set(epoch));
            notify_scope(&scope.store, scope.id, epoch, true, false);
        });
    }

    /// Records a successful check only if no dependency changed while checking.
    ///
    /// Returns false for a stale snapshot. Does not validate parents, invoke
    /// callbacks, or wake observers.
    pub(crate) fn set_ready(&self, generation: Generation) -> bool {
        if self.is_current(generation) {
            self.with(|s| s.ready.set(true));
            true
        } else {
            false
        }
    }

    /// Appends a persistent notifier subscription during service initialization.
    ///
    /// Does not deduplicate: repeated subscriptions cause repeated direct delivery.
    /// Panics if this handle is unbound or the source is delivering a notification.
    pub(crate) fn subscribe(&self, source: &ReadyNotifier) {
        let endpoint = self.endpoint();
        source.0.borrow_mut().push(endpoint);
    }

    /// Sets the callback payload before forwarding service binding.
    ///
    /// Does not invalidate readiness. Panics if the scope is unbound or retired.
    pub(crate) fn set_data(&self, data: usize) {
        self.with(|s| s.data.set(data));
    }

    /// Retains a callback until scope retirement, deduplicating its shared identity.
    ///
    /// Panics if the scope is unbound, retired, or currently delivering callbacks.
    pub(crate) fn on_notify(&self, callback: &ReadyCallback) {
        self.with(|s| {
            let mut callbacks = s.callbacks.borrow_mut();
            if !callbacks.iter().any(|c| Rc::ptr_eq(&c.0, &callback.0)) {
                callbacks.push(callback.clone());
            }
        });
    }

    /// Binds an unbound child to this store and records this scope as its parent.
    ///
    /// Invalidation propagates from child to parent. Repeated edges and
    /// self-dependencies are ignored; stale parent IDs need no cleanup.
    ///
    /// # Panics
    ///
    /// Panics if this handle is unbound, the child belongs to another store, or
    /// allocation or registration conflicts with borrowed scope or parent storage.
    pub(crate) fn depend_on(&self, child: &Self) {
        let parent = self.bound();
        let child = child.0.get_or_init(|| {
            let id = parent.store.scopes.allocate();
            ScopeRef {
                store: parent.store.clone(),
                id,
            }
        });
        assert!(
            Rc::ptr_eq(&parent.store, &child.store),
            "ReadyBinding cannot be shared across pipelines; use ReadyLink with a shared Pipeline"
        );
        if parent.id != child.id {
            parent
                .store
                .scopes
                .with(child.id, |s| link(&s.parents, parent.id));
        }
    }

    /// Verifies that the child is this scope or records this scope as a local parent.
    ///
    /// Panics for unbound handles, foreign stores, or missing dependency edges.
    pub(crate) fn assert_dependency(&self, child: &Self) {
        assert!(
            child.0.get().is_some_and(|child| {
                let parent = self.bound();
                Rc::ptr_eq(&parent.store, &child.store)
                    && (parent.id == child.id
                        || parent
                            .store
                            .scopes
                            .with(child.id, |s| s.parents.borrow().contains(&parent.id))
                            == Some(true))
            }),
            "ReadyBinding must be registered by Service::bind before use"
        );
    }

    /// Allocates an empty observer slot for one independently waiting task.
    ///
    /// The returned handle removes its slot on drop without retaining the scope.
    /// A waker is installed separately with [`ReadyObserver::register`].
    pub(crate) fn observer(&self) -> ReadyObserver {
        ReadyObserver {
            store: self.bound().store.clone(),
            scope: self.bound().id,
            idx: self.with(|s| s.observers.slots.borrow_mut().insert(None)),
        }
    }

    /// Returns the scope's shared, lazily allocated observer-forwarding waker.
    ///
    /// Waking it schedules observers without invalidating readiness and must
    /// happen on the owning thread, even after the scope has been retired.
    pub(crate) fn checker_waker(&self) -> Waker {
        let endpoint = self.endpoint();
        self.with(|s| s.observers.checker_waker(endpoint))
    }

    /// Drives or joins a shared check without allocating on a cached success.
    ///
    /// Only the current owner runs `check`; other callers wait for its result or
    /// cancellation. Successful checks cache readiness only for a current
    /// generation, while errors return to the owner without being cached.
    /// Completion or cancellation releases ownership and wakes waiting callers,
    /// which recheck the cache before claiming ownership themselves.
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
                } else if self.with(|s| s.checking.replace(true)) {
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
    ///
    /// Takes their wakers into a buffer and wakes them after releasing scope and
    /// observer-storage borrows. Observer slots remain available for registration.
    pub(crate) fn wake_observers(&self) {
        let mut wakers = Wakers::default();
        self.with(|s| s.observers.take_wakers(&mut wakers, None));
    }
}

#[derive(Debug)]
struct ScopeInner {
    owners: Cell<usize>,
    data: Cell<usize>,
    generation: Cell<Generation>,
    ready: Cell<bool>,
    checking: Cell<bool>,
    notify_epoch: Cell<u64>,
    direct_epoch: Cell<u64>,
    callbacks: RefCell<Vec<ReadyCallback>>,
    parents: RefCell<Vec<usize>>,
    external_parents: RefCell<Vec<Endpoint>>,
    observers: ReadyObservers,
}

impl Default for ScopeInner {
    /// Creates an unready scope with one owner and no registrations or dependencies.
    fn default() -> Self {
        Self {
            owners: Cell::new(1),
            data: Cell::new(0),
            generation: Cell::default(),
            ready: Cell::default(),
            checking: Cell::default(),
            notify_epoch: Cell::default(),
            direct_epoch: Cell::default(),
            callbacks: RefCell::default(),
            parents: RefCell::default(),
            external_parents: RefCell::default(),
            observers: ReadyObservers::default(),
        }
    }
}

impl ScopeInner {
    /// Advances the validation generation and clears cached readiness.
    ///
    /// Direct invalidation also advances the restart marker for pending checks.
    /// Does not traverse dependencies or deliver callbacks or wakes.
    ///
    /// Panics when the generation counter overflows.
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

#[derive(Debug, Default)]
struct ReadyObservers {
    slots: RefCell<slab::Slab<Option<Waker>>>,
    checker: OnceCell<Arc<CheckerWake>>,
    _local: PhantomData<Rc<()>>,
}

impl ReadyObservers {
    /// Returns the cached checker waker, creating it for this scope's endpoint.
    ///
    /// The endpoint must identify the scope owning these observers. Its waker
    /// forwards wakes only on the thread that first creates it.
    fn checker_waker(&self, endpoint: Endpoint) -> Waker {
        let checker = self.checker.get_or_init(|| {
            Arc::new(CheckerWake {
                owner: thread::current().id(),
                endpoint: mem::ManuallyDrop::new(endpoint),
                closed: Cell::new(false),
            })
        });
        Waker::from(checker.clone())
    }

    /// Disables any cached checker without waking tasks or removing observer slots.
    ///
    /// Repeated calls are harmless; retained closed wakers still enforce ownership.
    fn close(&self) {
        if let Some(checker) = self.checker.get() {
            checker.closed.set(true);
        }
    }

    /// Moves registered wakers into the delivery buffer without removing slots.
    ///
    /// Wakers matching `skip` data remain registered. Drop the buffer only after
    /// releasing scope and observer borrows, since delivery may reenter.
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
    /// Closes the cached checker before destroying the observer storage.
    fn drop(&mut self) {
        self.close();
    }
}

struct CheckerWake {
    owner: ThreadId,
    closed: Cell<bool>,
    endpoint: mem::ManuallyDrop<Endpoint>,
}

// SAFETY: Arc reference counts are atomic. The Rc endpoint is accessed and
// destroyed only on its owning thread; rejected destruction leaves it intact.
unsafe impl Send for CheckerWake {}
// SAFETY: The Cell and Rc endpoint are accessed only on the owning thread.
// Foreign-thread access, including Debug, only reads the immutable owner ID.
unsafe impl Sync for CheckerWake {}

impl CheckerWake {
    /// Checks thread ownership without accessing the thread-local endpoint.
    fn is_owner(&self) -> bool {
        thread::current().id() == self.owner
    }
}

impl fmt::Debug for CheckerWake {
    /// Formats only thread-safe metadata without inspecting the local endpoint.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckerWake")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl Drop for CheckerWake {
    /// Drops the endpoint only on its owning thread.
    ///
    /// Foreign-thread destruction leaks the endpoint and panics unless already
    /// unwinding, avoiding non-atomic reference-count access and a second panic.
    fn drop(&mut self) {
        if self.is_owner() {
            // SAFETY: The owner thread is the only thread that touches this Rc.
            unsafe { mem::ManuallyDrop::drop(&mut self.endpoint) };
        } else if !thread::panicking() {
            // Never drop the Rc on this thread, even while unwinding.
            panic!("ntex-service readiness observers can only be accessed on their owning thread");
        }
    }
}

impl Wake for CheckerWake {
    /// Consumes this handle and forwards to owner-thread observer delivery.
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    /// Wakes observers without invalidating readiness or waking this checker again.
    ///
    /// Closed checkers and retired scopes are ignored after checking ownership.
    ///
    /// # Panics
    ///
    /// Panics on a foreign thread, even if the checker has already been closed.
    fn wake_by_ref(self: &Arc<Self>) {
        assert!(
            self.is_owner(),
            "ntex-service readiness observers can only be accessed on their owning thread"
        );
        if self.closed.get() {
            return;
        }
        // Checks polled by this waker register it too. Waking it would only
        // repeat this pass, so it stays registered for later notifications.
        let this = Arc::as_ptr(self).cast::<()>();
        let mut wakers = Wakers::default();
        self.endpoint.store.scopes.with(self.endpoint.scope, |s| {
            s.observers.take_wakers(&mut wakers, Some(this));
        });
    }
}

struct CheckOwner<'a>(&'a ReadyScope);

impl Drop for CheckOwner<'_> {
    /// Releases shared-check ownership and wakes observers to retry or use the cache.
    fn drop(&mut self) {
        self.0.with(|s| s.checking.set(false));
        self.0.wake_observers();
    }
}

/// One task's registration, independent of the scope's persistent subscriptions.
#[derive(Debug)]
pub(crate) struct ReadyObserver {
    store: Rc<WaitersInner>,
    scope: usize,
    idx: usize,
}

impl ReadyObserver {
    /// Clears this task's waker without removing its reusable observer slot.
    ///
    /// Retired scopes are ignored; the removed waker is dropped outside all borrows.
    pub(crate) fn clear(&self) {
        let waker = self.store.scopes.with(self.scope, |s| {
            s.observers.slots.borrow_mut()[self.idx].take()
        });
        drop(waker);
    }

    /// Registers this task's waker before checking readiness to avoid missed wakes.
    ///
    /// Reuses an equivalent waker; otherwise clones and drops wakers outside
    /// storage borrows. Returns false if the scope retires before registration.
    pub(crate) fn register(&self, waker: &Waker) -> bool {
        let Some(registered) = self.store.scopes.with(self.scope, |s| {
            s.observers.slots.borrow()[self.idx]
                .as_ref()
                .is_some_and(|prev| prev.will_wake(waker))
        }) else {
            return false;
        };
        if registered {
            return true;
        }

        // Waker clone/drop implementations can reenter, just like wake.
        let mut waker = Some(waker.clone());
        let previous = self.store.scopes.with(self.scope, |s| {
            std::mem::replace(&mut s.observers.slots.borrow_mut()[self.idx], waker.take())
        });
        let registered = previous.is_some();
        drop(previous);
        registered
    }
}

impl Drop for ReadyObserver {
    /// Removes the task's slot if still live, dropping its waker outside all borrows.
    fn drop(&mut self) {
        let waker = self.store.scopes.with(self.scope, |s| {
            s.observers.slots.borrow_mut().remove(self.idx)
        });
        drop(waker);
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
    /// Creates an idle runner without invoking the check factory until polled.
    ///
    /// Does not acquire shared-check ownership or populate the readiness cache;
    /// callers handle those steps when caching a successful generation.
    pub(crate) fn new(scope: &'a ReadyScope, check: F) -> Self {
        Self {
            scope,
            check,
            fut: None,
            observer: None,
            generation: Generation::default(),
        }
    }

    /// Returns the scope whose invalidations govern this runner's checks.
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

    /// Polls until a successful check belongs to an unchanged generation.
    ///
    /// Direct invalidation replaces an outstanding check. Propagated invalidation
    /// lets it finish, but retries stale successes; errors return without retry.
    /// Repeated invalidation yields to the executor instead of spinning.
    /// After completion, the next poll starts a new check.
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
