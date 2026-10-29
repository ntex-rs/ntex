//! Owner-directed references. Remote code reads immutable routing and sets a
//! sticky publication flag, but never accesses owner-only execution state.
use std::process::abort;
use std::sync::{Arc, atomic::Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::{cell::Cell, cell::RefCell, collections::HashMap, mem::ManuallyDrop, ptr, ptr::NonNull};

#[cfg(ntex_loom)]
use loom::sync::RwLock;
#[cfg(not(ntex_loom))]
use std::sync::{OnceLock, RwLock};

use super::task::{self, Core, Header, Remote, Runnable};
use super::{ThreadId, queue::RunnableQueue, try_current_thread_id};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RuntimeId(u16);

/// Standard wakers own owner-accounted references to the task allocation.
static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake::<true>, wake::<false>, drop_waker);

// Entries are populated only on their owner thread, so a matching never-reused
// runtime ID proves ownership. Shutdown invalidates the entry before callbacks.
#[derive(Clone, Copy)]
enum OwnerEndpoint {
    Unknown,
    Active(RuntimeId),
    Closed(RuntimeId),
}

enum Owner {
    Active,
    Closed,
    Remote,
}

#[cfg(not(ntex_loom))]
type RemoteEndpoints = crate::HashMap<RuntimeId, Arc<RunnableQueue>>;

// Loom must replay the same pruning order when it models admission loads.
#[cfg(ntex_loom)]
type RemoteEndpoints = HashMap<RuntimeId, Arc<RunnableQueue>, foldhash::fast::FixedState>;

#[cfg(not(ntex_loom))]
thread_local! {
    static OWNER_ENDPOINT: Cell<OwnerEndpoint> = const { Cell::new(OwnerEndpoint::Unknown) };
    static REMOTE_ENDPOINTS: RefCell<RemoteEndpoints> = RefCell::new(RemoteEndpoints::default());
    #[cfg(test)]
    pub(super) static REGISTRY_LOOKUPS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(ntex_loom)]
loom::thread_local! {
    static OWNER_ENDPOINT: Cell<OwnerEndpoint> = Cell::new(OwnerEndpoint::Unknown);
    static REMOTE_ENDPOINTS: RefCell<RemoteEndpoints> = RefCell::new(RemoteEndpoints::default());
}

struct Registry {
    /// The last issued runtime identity.
    next: u16,
    queues: HashMap<RuntimeId, Arc<RunnableQueue>>,
}

impl Registry {
    /// Creates an empty registry with no runtime identities issued.
    fn new() -> Self {
        Self {
            next: 0,
            queues: HashMap::new(),
        }
    }
}

/// Returns the process-wide registry, initializing it on first use.
#[cfg(not(ntex_loom))]
fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(Registry::new()))
}

/// Returns the registry scoped to the current Loom model execution.
#[cfg(ntex_loom)]
fn registry() -> &'static RwLock<Registry> {
    loom::lazy_static! {
        static ref REGISTRY: RwLock<Registry> = RwLock::new(Registry::new());
    }
    &REGISTRY
}

/// Allocates a never-reused runtime identity, aborting on exhaustion.
///
/// Identities are limited to `u16::MAX` per process.
pub(crate) fn runtime_id() -> RuntimeId {
    let mut registry = registry().write().unwrap();
    registry.next = registry.next.checked_add(1).unwrap_or_else(|| {
        log::error!(
            target: crate::rt::LOG_TARGET,
            "Runtime identities are exhausted, {} runtimes per process",
            u16::MAX
        );
        abort()
    });
    RuntimeId(registry.next)
}

/// Sets the last issued runtime identity, to test exhaustion.
#[cfg(all(test, not(ntex_loom)))]
pub(crate) fn set_runtime_index(last: u16) {
    registry().write().unwrap().next = last;
}

/// Publishes a runtime's wake endpoint, rejecting duplicate registration.
pub(crate) fn register(queue: &Arc<RunnableQueue>) {
    let old = registry()
        .write()
        .unwrap()
        .queues
        .insert(queue.runtime_id, queue.clone());
    assert!(old.is_none(), "runtime registered twice");
}

/// Keeps a closed endpoint available for owner cleanup until shutdown has drained.
#[must_use]
pub(crate) struct Shutdown(RuntimeId);

impl Drop for Shutdown {
    fn drop(&mut self) {
        let removed = registry().write().unwrap().queues.remove(&self.0);
        // Endpoint destruction, like notification, must be outside the lock.
        drop(removed);
    }
}

/// Rejects owner wakers and waits for endpoint lookups before shutdown runs callbacks.
///
/// Admission must already be closed. The registry retains the queue for owner
/// releases during draining; dropping the guard removes that cleanup access.
#[allow(
    clippy::readonly_write_lock,
    reason = "shutdown synchronizes in-flight endpoint lookups"
)]
pub(crate) fn unregister(id: RuntimeId) -> Shutdown {
    OWNER_ENDPOINT.with(|cached| cached.set(OwnerEndpoint::Closed(id)));
    let registry = registry().write().unwrap();
    debug_assert!(
        registry
            .queues
            .get(&id)
            .is_some_and(|queue| queue.is_closed())
    );
    Shutdown(id)
}

/// Coallocated immutable metadata, readable without borrowing the task header.
pub(super) struct Routing {
    pub(super) owner: ThreadId,
    pub(super) runtime: RuntimeId,
}

/// Projects immutable routing metadata without borrowing owner-only task state.
///
/// # Safety
///
/// `data` must point to a native task allocation that remains valid for `'a`.
unsafe fn routing<'a>(data: *const ()) -> &'a Routing {
    unsafe { &(*data.cast::<Core>()).routing }
}

/// Checks whether this task still has a live endpoint on the current thread.
#[inline]
pub(super) unsafe fn active_owner(data: *const ()) -> bool {
    matches!(unsafe { owner(data) }, Owner::Active)
}

/// Clones a native reference on its owner thread.
///
/// The caller must hold a native task reference and be on its owner thread.
#[inline]
pub(crate) unsafe fn clone_local_waker(ptr: *const ()) {
    task::inc_local(unsafe { task::task_ptr(ptr) });
}

/// Wakes a native task on its owner thread, transferring a reference if owned.
///
/// The caller must hold a native task reference and be on its owner thread.
#[inline]
pub(crate) unsafe fn wake_local_waker(ptr: *const (), owned: bool) {
    task::wake_local::<true>(unsafe { task::task_ptr(ptr) }, owned);
}

/// Releases a native reference on its owner thread.
///
/// The caller must own a native task reference and be on its owner thread.
#[inline]
pub(crate) unsafe fn drop_local_waker(ptr: *const ()) {
    task::release_unshared(unsafe { task::task_ptr(ptr) });
}

/// Borrows a task reference; the raw pointer retains allocation provenance.
#[inline]
pub(crate) fn raw_waker(ptr: *const ()) -> RawWaker {
    RawWaker::new(ptr, &WAKER_VTABLE)
}

/// Acquire thread-safe ownership before releasing the local reference.
pub(crate) unsafe fn into_waker(ptr: *const ()) -> Waker {
    let raw = ManuallyDrop::new(unsafe { Waker::from_raw(raw_waker(ptr)) });
    let waker = (*raw).clone();
    unsafe { drop_local_waker(ptr) };
    waker
}

/// Resolves owner-local liveness, caching closure before shutdown runs callbacks.
#[inline]
unsafe fn owner(data: *const ()) -> Owner {
    let route = unsafe { routing(data) };
    match OWNER_ENDPOINT.with(Cell::get) {
        OwnerEndpoint::Active(id) if id == route.runtime => Owner::Active,
        OwnerEndpoint::Closed(id) if id == route.runtime => Owner::Closed,
        _ => refresh_owner(route),
    }
}

/// Resolves a cache miss without keeping registry lookup state on the hit path.
#[cold]
fn refresh_owner(route: &Routing) -> Owner {
    if Some(route.owner) != try_current_thread_id() {
        return Owner::Remote;
    }
    let id = route.runtime;
    if with_registered(id, |_| ()).is_some() {
        OWNER_ENDPOINT.with(|cached| cached.set(OwnerEndpoint::Active(id)));
        Owner::Active
    } else {
        OWNER_ENDPOINT.with(|cached| cached.set(OwnerEndpoint::Closed(id)));
        Owner::Closed
    }
}

/// Borrows an open endpoint under the registry guard; `f` must not run callbacks.
///
/// Retained endpoints must reserve admission before publishing.
pub(super) fn with_registered<R>(
    id: RuntimeId,
    f: impl FnOnce(&Arc<RunnableQueue>) -> R,
) -> Option<R> {
    #[cfg(all(test, not(ntex_loom)))]
    REGISTRY_LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));

    let registry = registry().read().unwrap();
    let queue = registry.queues.get(&id)?;
    if queue.is_closed() { None } else { Some(f(queue)) }
}

/// Retains an active or draining endpoint for owner cleanup, including callbacks.
///
/// Unlike remote lookup, closed queues remain available until shutdown drains.
/// Open TLS entries bypass the registry without populating or pruning the cache.
pub(super) fn retain_owner_queue(id: RuntimeId) -> Option<Arc<RunnableQueue>> {
    if let Ok(Some(queue)) = REMOTE_ENDPOINTS.try_with(|cached| {
        cached
            .borrow()
            .get(&id)
            .filter(|queue| !queue.is_closed())
            .cloned()
    }) {
        return Some(queue);
    }

    #[cfg(all(test, not(ntex_loom)))]
    REGISTRY_LOOKUPS.with(|lookups| lookups.set(lookups.get() + 1));

    registry().read().unwrap().queues.get(&id).cloned()
}

/// Retains a remote endpoint observed open, consulting TLS before the registry.
///
/// Lookup does not publish work or reserve admission. The endpoint may close
/// immediately afterward, so callers must still admit work before publishing.
pub(super) fn remote_queue(id: RuntimeId) -> Option<Arc<RunnableQueue>> {
    let queue = match REMOTE_ENDPOINTS.try_with(|cached| cached.borrow().get(&id).cloned()) {
        Ok(Some(queue)) => queue,
        _ => remote_queue_uncached(id)?,
    };
    if queue.is_closed() {
        let removed = REMOTE_ENDPOINTS.try_with(|cached| cached.borrow_mut().remove(&id));
        drop(removed);
        None
    } else {
        Some(queue)
    }
}

/// Publishes through the remote thread's cached endpoints, notifying outside TLS borrows.
///
/// Returns `false` if the endpoint is absent or has closed admission.
fn publish(id: RuntimeId, entry: WakeEntry) -> bool {
    let mut entry = Some(entry);
    let cached = REMOTE_ENDPOINTS.try_with(|cached| {
        let cached = cached.borrow();
        cached
            .get(&id)
            .map(|queue| publish_admitted(queue, entry.take().unwrap()))
    });
    let notify = match cached {
        Ok(Some(notify)) => notify,
        _ => remote_queue(id)
            .ok_or(())
            .and_then(|queue| publish_admitted(&queue, entry.unwrap())),
    };
    let Ok(notify) = notify else {
        let removed = REMOTE_ENDPOINTS.try_with(|cached| cached.borrow_mut().remove(&id));
        drop(removed);
        return false;
    };
    if let Some(queue) = notify {
        queue.notify();
    }
    true
}

/// Caches a new endpoint and prunes closed ones, or bypasses unavailable TLS.
#[cold]
fn remote_queue_uncached(id: RuntimeId) -> Option<Arc<RunnableQueue>> {
    let queue = with_registered(id, Arc::clone);
    if let Ok(retired) = REMOTE_ENDPOINTS.try_with(|cached| {
        let mut cached = cached.borrow_mut();
        let mut retired: Vec<_> = cached
            .extract_if(|_, queue| queue.is_closed())
            .map(|(_, queue)| queue)
            .collect();
        if let Some(queue) = &queue {
            retired.extend(cached.insert(id, queue.clone()));
        }
        retired
    }) {
        // Destructors may reenter wakers. Keep our endpoint alive independently,
        // release the TLS borrow, and retire closed endpoints before returning.
        drop(retired);
    }
    queue
}

/// Reserves shutdown-safe publication without cloning the cached queue reference.
fn publish_admitted(
    queue: &Arc<RunnableQueue>,
    entry: WakeEntry,
) -> Result<Option<Arc<RunnableQueue>>, ()> {
    let permit = queue.admit(false).ok_or(())?;
    // Project only the shared atomic, without borrowing the owner-only core.
    // The source reference protects this store and the following publication.
    let touched = unsafe { &(*entry.header().cast::<Core>().as_ptr()).remote_touched };
    // Acquiring an existing mark carries it through a returned remote clone.
    if !touched.load(Ordering::Acquire) {
        touched.store(true, Ordering::Release);
    }
    Ok(permit.publish(entry))
}

/// Each entry transfers a counted reference, or borrows a source whose release
/// follows its publication. The consumer must process entries in FIFO order.
///
/// On 64-bit targets, the operation occupies three alignment bits in the pointer.
/// Only `header()` may recover a pointer for accessing task storage.
pub(crate) struct WakeEntry {
    data: NonNull<Header>,
    #[cfg(not(target_pointer_width = "64"))]
    op: Operation,
}

#[cfg(target_pointer_width = "64")]
const OP_MASK: usize = 7;

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(std::mem::align_of::<Header>() > OP_MASK);
    assert!(std::mem::size_of::<WakeEntry>() == std::mem::size_of::<usize>());
};

#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum Operation {
    /// Activates an unpolled, detached submission.
    Spawn = 0,
    AddRef = 1,
    Release = 2,
    Wake = 3,
    WakeAndRelease = 4,
}

unsafe impl Send for WakeEntry {}

impl WakeEntry {
    /// Encodes a command without exposing or discarding the pointer's provenance.
    pub(super) fn new(h: NonNull<Header>, op: Operation) -> Self {
        #[cfg(target_pointer_width = "64")]
        let h = {
            debug_assert_eq!(h.addr().get() & OP_MASK, 0);
            h.map_addr(|addr| addr | op as usize)
        };
        Self {
            data: h,
            #[cfg(not(target_pointer_width = "64"))]
            op,
        }
    }

    /// Removes the operation tag before any task-field projection.
    fn header(&self) -> NonNull<Header> {
        #[cfg(target_pointer_width = "64")]
        return self.data.map_addr(|addr| {
            // Clearing only the tag restores the original aligned, non-null pointer.
            unsafe { std::num::NonZeroUsize::new_unchecked(addr.get() & !OP_MASK) }
        });
        #[cfg(not(target_pointer_width = "64"))]
        return self.data;
    }

    /// Decodes the command while leaving its task reference untouched.
    fn operation(&self) -> Operation {
        #[cfg(target_pointer_width = "64")]
        return match self.data.addr().get() & OP_MASK {
            0 => Operation::Spawn,
            1 => Operation::AddRef,
            2 => Operation::Release,
            3 => Operation::Wake,
            4 => Operation::WakeAndRelease,
            _ => unreachable!("invalid wake operation"),
        };
        #[cfg(not(target_pointer_width = "64"))]
        return self.op;
    }

    /// Transfers a submission's reference to the owner's FIFO.
    pub(crate) fn spawn(task: Remote) -> Self {
        Self::new(ManuallyDrop::new(task).0, Operation::Spawn)
    }

    /// Takes back the submission of a `Spawn` entry.
    #[cfg(test)]
    pub(crate) fn into_remote(self) -> Remote {
        assert!(
            matches!(self.operation(), Operation::Spawn),
            "not a submission"
        );
        Remote(self.header())
    }

    /// Applies one FIFO operation on the owner, returning a newly scheduled runnable.
    pub(crate) fn into_runnable(self, queue: &RunnableQueue) -> Option<Runnable> {
        let h = self.header();
        let op = self.operation();
        debug_assert_eq!(task::core(h).routing.runtime, queue.runtime_id);
        match op {
            Operation::Spawn => return Some(Remote(h).into_runnable()),
            Operation::AddRef => task::inc_local(h),
            Operation::Release => task::release_local_ordered(h),
            Operation::Wake | Operation::WakeAndRelease => {
                queue.acquire_wakes();
                let runnable = task::mark_scheduled(h).then(|| {
                    // Establish scheduling ownership before consuming the waker.
                    task::inc_local(h);
                    Runnable(h)
                });
                if matches!(op, Operation::WakeAndRelease) {
                    task::release_local_ordered(h);
                }
                return runnable;
            }
        }
        None
    }
}

/// Checks routing once, for a direct owner-local handle.
#[inline]
pub(crate) fn local_waker_ptr(waker: &Waker) -> Option<*const ()> {
    if ptr::eq(waker.vtable(), &raw const WAKER_VTABLE) && unsafe { active_owner(waker.data()) } {
        Some(waker.data())
    } else {
        None
    }
}

/// Acquires a standard-waker reference locally or through the FIFO; closed endpoints
/// yield an inert waker.
///
/// # Safety
///
/// `data` must belong to a live native standard waker or a protected borrowed poll waker.
unsafe fn clone_waker(data: *const ()) -> RawWaker {
    let h = unsafe { task::task_ptr(data) };
    let acquired = match unsafe { owner(data) } {
        Owner::Active => {
            task::inc_local(h);
            true
        }
        Owner::Closed => false,
        Owner::Remote => publish(
            unsafe { routing(data) }.runtime,
            WakeEntry::new(h, Operation::AddRef),
        ),
    };
    if acquired {
        raw_waker(data)
    } else {
        RawWaker::new(Waker::noop().data(), Waker::noop().vtable())
    }
}

/// Wakes a task, consuming the standard-waker reference if `OWNED`.
/// Closed endpoints are ignored.
///
/// # Safety
///
/// `data` must belong to a live native standard waker, owned if `OWNED`.
/// A borrowed poll waker is also valid when `OWNED` is false.
unsafe fn wake<const OWNED: bool>(data: *const ()) {
    let h = unsafe { task::task_ptr(data) };
    match unsafe { owner(data) } {
        // Scheduling may notify a driver that reenters shutdown.
        // Nothing may touch the task afterward.
        Owner::Active => task::wake_local::<false>(h, OWNED),
        Owner::Closed => {}
        Owner::Remote => {
            publish(
                unsafe { routing(data) }.runtime,
                WakeEntry::new(
                    h,
                    if OWNED {
                        Operation::WakeAndRelease
                    } else {
                        Operation::Wake
                    },
                ),
            );
        }
    }
}

/// Releases a standard-waker reference locally or through the FIFO, unless unregistered.
///
/// # Safety
///
/// `data` must belong to an owned native standard waker whose reference is consumed.
unsafe fn drop_waker(data: *const ()) {
    let h = unsafe { task::task_ptr(data) };
    match unsafe { owner(data) } {
        Owner::Active => task::release_local(h),
        Owner::Closed => {}
        Owner::Remote => {
            publish(
                unsafe { routing(data) }.runtime,
                WakeEntry::new(h, Operation::Release),
            );
        }
    }
}

pub(crate) struct WakeQueue {
    #[cfg(not(ntex_loom))]
    queue: crossbeam_queue::SegQueue<WakeEntry>,
    #[cfg(ntex_loom)]
    queue: loom::sync::Mutex<std::collections::VecDeque<WakeEntry>>,
}

impl WakeQueue {
    /// Creates an empty reference-and-wake command FIFO.
    pub(crate) fn new() -> Self {
        Self {
            #[cfg(not(ntex_loom))]
            queue: crossbeam_queue::SegQueue::new(),
            #[cfg(ntex_loom)]
            queue: loom::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Publishes a command from an owner or remote producer.
    pub(crate) fn push(&self, entry: WakeEntry) {
        #[cfg(not(ntex_loom))]
        self.queue.push(entry);
        #[cfg(ntex_loom)]
        self.queue.lock().unwrap().push_back(entry);
    }

    /// Removes the oldest command for processing by the owner.
    pub(crate) fn pop(&self) -> Option<WakeEntry> {
        #[cfg(not(ntex_loom))]
        return self.queue.pop();
        #[cfg(ntex_loom)]
        return self.queue.lock().unwrap().pop_front();
    }

    /// Returns whether the FIFO currently contains no published commands.
    pub(crate) fn is_empty(&self) -> bool {
        #[cfg(not(ntex_loom))]
        return self.queue.is_empty();
        #[cfg(ntex_loom)]
        return self.queue.lock().unwrap().is_empty();
    }
}
