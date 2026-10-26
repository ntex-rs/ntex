//! Task type of the neon runtime.
//!
//! A task is polled only on the thread of its runtime, the owner thread.
//! Wakers retain this allocation, but only the owner updates their references.
//!
//! The owner keeps task state, join results and all reference counts in plain
//! cells. Remote submissions transfer one unpolled, detached task to the owner.
//! Remote wakers set a sticky publication flag and publish typed FIFO operations
//! without touching owner-only task state.
//!
//! The local runtime queue is intrusive, a task links itself into it. Remote
//! submissions share the FIFO of remote wake operations. The runtime also lists
//! its unfinished tasks, it drops their futures when it is dropped. The list is
//! non-owning.
//!
//! `TaskCore` is pinned. Its header, awaiter and future/output storage are
//! owner-only after submission. Immutable routing selects a runtime endpoint;
//! remote wakers never read the header. Shutdown closes that endpoint and ignores
//! late standard-waker operations, permanently retaining their storage. The queue
//! pointer may dangle afterward; owner cleanup resolves liveness by runtime ID.
use std::cell::Cell;
use std::marker::{PhantomData, PhantomPinned};
use std::sync::{Arc, atomic::Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::{fmt, future::Future, mem, mem::ManuallyDrop, pin::Pin};
use std::{process::abort, ptr, ptr::NonNull};

use crate::handle::SpawnLocation;
use crate::rt::{REPORTED, RunnableQueue, TaskStats, try_current_thread_id};

#[path = "task_wake.rs"]
pub(super) mod wake;
pub(crate) use wake::{WakeEntry, WakeQueue};

#[cfg(ntex_loom)]
use loom::sync::atomic::AtomicBool;
#[cfg(not(ntex_loom))]
use std::sync::atomic::AtomicBool;

/// Owner-only storage after submission; Loom also tracks transfer and reentrancy.
struct SharedCell<T>(
    #[cfg(not(ntex_loom))] std::cell::UnsafeCell<T>,
    #[cfg(ntex_loom)] loom::cell::UnsafeCell<T>,
);

impl<T> SharedCell<T> {
    /// Creates owner-local storage, tracked by Loom when enabled.
    fn new(v: T) -> Self {
        #[cfg(not(ntex_loom))]
        return Self(std::cell::UnsafeCell::new(v));
        #[cfg(ntex_loom)]
        return Self(loom::cell::UnsafeCell::new(v));
    }

    /// The pointer must only be accessed within the closure.
    #[inline]
    fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
        #[cfg(not(ntex_loom))]
        return f(self.0.get());
        #[cfg(ntex_loom)]
        return self.0.with_mut(f);
    }
}

// owner-thread state
/// Queued in the local queue, or woken while running.
const SCHEDULED: u8 = 1;
const RUNNING: u8 = 1 << 1;
/// The future is dropped.
const FINISHED: u8 = 1 << 2;
/// Cancel requested, the runner drops the future.
const CLOSE: u8 = 1 << 3;
/// The self-wake streak is reported.
const WAKES_REPORTED: u8 = 1 << 4;
/// No join handle, the output is dropped on completion.
const DETACHED: u8 = 1 << 5;
/// In the task list of the runtime.
const LISTED: u8 = 1 << 6;
/// The self-wake count saturates at this value.
pub(crate) const MAX_SELF_WAKES: u32 = u16::MAX as u32;

/// Functions of a task that depend on the future type.
struct VTable {
    /// Polls the future, on completion drops it and stores the output.
    poll: unsafe fn(NonNull<Header>, &mut Context<'_>) -> bool,
    drop_future: unsafe fn(NonNull<Header>),
    /// Moves the output to `*mut Option<T>`.
    take_output: unsafe fn(NonNull<Header>, *mut ()),
    drop_output: unsafe fn(NonNull<Header>),
    dealloc: unsafe fn(NonNull<Header>),
}

/// Owner-only header, initialized before transferring an unpolled submission.
#[repr(C)]
struct Header {
    vtable: &'static VTable,
    /// Owner-only; may dangle after shutdown, retaining no queue storage or driver.
    queue: NonNull<RunnableQueue>,
    /// Where the task is spawned, for logs and stats.
    location: SpawnLocation,
    /// Waker of the join handle.
    awaiter: Awaiter,
}

/// Type-erased prefix of the pinned core. Execution fields are owner-only.
///
/// Fields packed without padding keep the core at 80 bytes. Fields used by wakes,
/// runs and releases fill the first 64 bytes; the task list links, used only on
/// spawn, completion and shutdown, come last.
#[repr(C)]
struct Core {
    header: Header,
    /// Local queue link.
    next: Cell<*mut Header>,
    /// Owner-only references from runnables, joins and wake handles.
    refs: Cell<u32>,
    /// Flags.
    state: Cell<u8>,
    /// Set before any remote waker publication; never cleared, even after draining.
    remote_touched: AtomicBool,
    routing: wake::Routing,
    /// Consecutive self-wakes, saturating at `MAX_SELF_WAKES`.
    self_wakes: Cell<u16>,
    /// Start of the self-wake streak, milliseconds since the runtime epoch.
    streak_start: Cell<u32>,
    /// Non-owning task list links.
    list_prev: Cell<*mut Header>,
    list_next: Cell<*mut Header>,
    _pin: PhantomPinned,
}

#[cfg(all(not(ntex_loom), target_pointer_width = "64"))]
const _: () = {
    assert!(mem::size_of::<Header>() == 40);
    assert!(mem::size_of::<wake::Routing>() == 4);
    assert!(mem::size_of::<Core>() == 80);
    assert!(mem::offset_of!(Core, list_prev) == 64);
};

enum Stage<F: Future> {
    /// The future, pinned in place.
    Running(F),
    /// The output, until the join handle takes or drops it.
    Finished(F::Output),
    /// The future is dropped, and the output is taken, dropped or never
    /// produced.
    Consumed,
}

/// Pinned allocation of a task. Both prefixes start at offset zero, so the
/// header pointer also identifies the core and its allocation.
#[repr(C)]
struct TaskCore<F: Future> {
    core: Core,
    stage: SharedCell<Stage<F>>,
    #[cfg(test)]
    _allocation: Arc<()>,
}

/// Functions of the vtable for the future type `F`.
struct Fns<F>(PhantomData<F>);

impl<F: Future> Fns<F> {
    const VTABLE: VTable = VTable {
        poll: Self::poll,
        drop_future: Self::drop_future,
        take_output: Self::take_output,
        drop_output: Self::drop_output,
        dealloc: Self::dealloc,
    };

    /// Gives `f` the stage, `h` must be a task of the future type `F`.
    #[inline]
    unsafe fn with_stage<R>(h: NonNull<Header>, f: impl FnOnce(*mut Stage<F>) -> R) -> R {
        unsafe { (*h.cast::<TaskCore<F>>().as_ptr()).stage.with_mut(f) }
    }

    /// Polls the pinned future and stores its output, returning whether it completed.
    ///
    /// # Safety
    ///
    /// `h` must identify a live task of type `F` on its owner thread, with its
    /// stage in `Running` and no overlapping access to the future.
    unsafe fn poll(h: NonNull<Header>, cx: &mut Context<'_>) -> bool {
        unsafe {
            let polled = Self::with_stage(h, |stage| {
                let Stage::Running(fut) = &mut *stage else {
                    unreachable!()
                };
                Pin::new_unchecked(fut).poll(cx)
            });
            match polled {
                Poll::Ready(out) => {
                    Self::drop_future(h);
                    Self::with_stage(h, |stage| ptr::write(stage, Stage::Finished(out)));
                    true
                }
                Poll::Pending => false,
            }
        }
    }

    /// Drops the pinned future in place and marks its stage consumed, even on panic.
    ///
    /// # Safety
    ///
    /// `h` must identify a live task of type `F` on its owner thread, with no
    /// overlapping access to its stage.
    unsafe fn drop_future(h: NonNull<Header>) {
        // the future is pinned, it is dropped in place
        struct Guard<F: Future>(*mut Stage<F>);

        impl<F: Future> Drop for Guard<F> {
            /// Leaves no initialized stage behind after its destructor runs or panics.
            fn drop(&mut self) {
                unsafe { ptr::write(self.0, Stage::Consumed) };
            }
        }

        unsafe {
            Self::with_stage(h, |stage| {
                let guard = Guard(stage);
                ptr::drop_in_place(guard.0);
            });
        }
    }

    /// Moves a stored result into the caller's typed output slot.
    ///
    /// # Safety
    ///
    /// `h` must identify a live task of type `F` on its owner thread.
    /// `out` must exclusively point to an initialized `Option<F::Output>`.
    unsafe fn take_output(h: NonNull<Header>, out: *mut ()) {
        if let Some(v) = unsafe { Self::take_output_value(h) } {
            unsafe { *out.cast::<Option<F::Output>>() = Some(v) };
        }
    }

    /// Takes and drops any stored result, leaving the stage consumed.
    ///
    /// # Safety
    ///
    /// `h` must identify a live task of type `F` on its owner thread, with no
    /// overlapping access to its stage.
    unsafe fn drop_output(h: NonNull<Header>) {
        drop(unsafe { Self::take_output_value(h) });
    }

    /// Takes the output if there is one, the stage becomes `Consumed`.
    unsafe fn take_output_value(h: NonNull<Header>) -> Option<F::Output> {
        unsafe {
            Self::with_stage(h, |stage| {
                if matches!(*stage, Stage::Finished(_))
                    && let Stage::Finished(v) = ptr::replace(stage, Stage::Consumed)
                {
                    Some(v)
                } else {
                    None
                }
            })
        }
    }

    /// Frees the task, drops a left future, output and awaiter.
    unsafe fn dealloc(h: NonNull<Header>) {
        drop(unsafe { Box::from_raw(h.cast::<TaskCore<F>>().as_ptr()) });
    }
}

/// Waker of the join handle.
struct Awaiter(SharedCell<Option<crate::LocalWaker>>);

impl Awaiter {
    /// Creates an empty owner-local join notification slot.
    fn new() -> Self {
        Self(SharedCell::new(None))
    }

    /// Takes the stored waker.
    ///
    /// # Safety
    ///
    /// The caller should have exclusive access to the awaiter.
    unsafe fn take(&self) -> Option<crate::LocalWaker> {
        self.0.with_mut(|p| unsafe { (*p).take() })
    }

    /// Stores `waker`, returns the replaced waker.
    ///
    /// Cloning `waker` and dropping the replaced waker may run user code,
    /// the replaced waker is dropped by the caller once the awaiter is
    /// consistent.
    ///
    /// # Safety
    ///
    /// The caller should have exclusive access to the awaiter.
    unsafe fn register(&self, waker: &Waker) -> Option<crate::LocalWaker> {
        if self.0.with_mut(|p| unsafe {
            (*p).as_ref()
                .is_some_and(|current| current.will_wake(waker))
        }) {
            return None;
        }
        // Clone outside the cell access: a custom callback can complete the task.
        let new = crate::LocalWaker::from_waker(waker);
        self.0.with_mut(|p| unsafe { (*p).replace(new) })
    }
}

impl Drop for Awaiter {
    /// Releases a stored join waker after removing it from the slot.
    fn drop(&mut self) {
        drop(unsafe { self.take() });
    }
}

/// Standard wakers own owner-accounted references to the task allocation.
static WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    wake::clone_waker,
    wake::wake,
    wake::wake_by_ref,
    wake::drop_waker,
);

/// Checks routing once, for a direct owner-local handle.
#[inline]
pub(crate) fn local_waker_ptr(waker: &Waker) -> Option<*const ()> {
    if ptr::eq(waker.vtable(), &raw const WAKER_VTABLE)
        && unsafe { wake::active_owner(waker.data()) }
    {
        Some(waker.data())
    } else {
        None
    }
}

/// Clones a native reference on its owner thread.
///
/// The caller must hold a native task reference and be on its owner thread.
#[inline]
pub(crate) unsafe fn clone_local_waker(ptr: *const ()) {
    inc_local(unsafe { task_ptr(ptr) });
}

/// Wakes a native task on its owner thread, transferring a reference if owned.
///
/// The caller must hold a native task reference and be on its owner thread.
#[inline]
pub(crate) unsafe fn wake_local_waker(ptr: *const (), owned: bool) {
    wake_local(unsafe { task_ptr(ptr) }, owned);
}

/// Releases a native reference on its owner thread.
///
/// The caller must own a native task reference and be on its owner thread.
#[inline]
pub(crate) unsafe fn drop_local_waker(ptr: *const ()) {
    release_local(unsafe { task_ptr(ptr) });
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

/// Header of a task, the caller holds a reference for the lifetime.
#[inline]
fn header<'a>(h: NonNull<Header>) -> &'a Header {
    unsafe { h.as_ref() }
}

/// The caller is on the owner thread and holds a task reference for the lifetime.
#[inline]
fn core<'a>(h: NonNull<Header>) -> &'a Core {
    unsafe { h.cast::<Core>().as_ref() }
}

/// Returns `true` on the owner thread of the task.
#[inline]
fn is_owner(h: NonNull<Header>) -> bool {
    Some(unsafe { (*h.cast::<Core>().as_ptr()).routing.owner }) == try_current_thread_id()
}

/// Borrows the live future's queue, or a completed task's still-registered endpoint.
///
/// The caller must establish liveness independently of the pointer. Completed-task
/// callers check registration and use the borrow only for callback-free operations.
fn owner_queue<'a>(h: NonNull<Header>) -> &'a RunnableQueue {
    unsafe { header(h).queue.as_ref() }
}

/// Borrows a live owner endpoint for callback-free inspection or publication.
fn live_owner_queue<'a>(h: NonNull<Header>) -> Option<&'a RunnableQueue> {
    if core(h).state.get() & FINISHED == 0 || unsafe { wake::active_owner(h.as_ptr().cast()) } {
        Some(owner_queue(h))
    } else {
        None
    }
}

/// Checks for FIFO commands that an owner release must not overtake.
fn has_pending_commands(h: NonNull<Header>) -> bool {
    if let Some(queue) = live_owner_queue(h) {
        !queue.wake_queue.is_empty()
    } else {
        wake::retain_owner_queue(h).is_some_and(|queue| !queue.wake_queue.is_empty())
    }
}

/// Task of a waker data pointer.
#[inline]
unsafe fn task_ptr(ptr: *const ()) -> NonNull<Header> {
    unsafe { NonNull::new_unchecked(ptr.cast_mut().cast::<Header>()) }
}

/// Adds a local reference on the owner thread, aborts on overflow.
#[inline]
fn inc_local(h: NonNull<Header>) {
    let refs = &core(h).refs;
    match refs.get().checked_add(1) {
        Some(n) => refs.set(n),
        None => abort(),
    }
}

/// Wakes the task on the owner thread, `owned` passes a reference.
fn wake_local(h: NonNull<Header>, owned: bool) {
    let local = core(h);
    let state = local.state.get();
    if state & (SCHEDULED | FINISHED) == 0 {
        local.state.set(state | SCHEDULED);
        // a running task is queued by the runner after the poll
        if state & RUNNING == 0 {
            if !owned {
                inc_local(h);
            }
            owner_queue(h).schedule_local(Runnable(h));
            return;
        }
    }
    if owned {
        release_local(h);
    }
}

/// Releases a reference on the owner thread.
#[inline]
fn release_local(h: NonNull<Header>) {
    // An unaccounted reference returning from a remote clone observes its sticky
    // store through the transfer. Otherwise a publisher's counted source keeps
    // this from being the final reference. Consuming a command also acquires its
    // preceding store. Thus an untouched task can release without FIFO ordering.
    if core(h).remote_touched.load(Ordering::Acquire) {
        release_touched(h);
    } else {
        release_local_ordered(h);
    }
}

/// Orders remotely touched releases without expanding the untouched fast path.
#[inline(never)]
fn release_touched(h: NonNull<Header>) {
    if !has_pending_commands(h) && core(h).refs.get() > 1 {
        release_local_ordered(h);
    } else {
        wake::defer_release(h);
    }
}

/// A queued final release must not enqueue itself again.
#[inline]
fn release_local_ordered(h: NonNull<Header>) {
    let local = core(h);
    let refs = local.refs.get().checked_sub(1).unwrap();
    local.refs.set(refs);
    if refs == 0 {
        last_ref(h);
    }
}

/// The ordered owner count reached zero: reclaim storage or cancel an orphan.
#[cold]
fn last_ref(h: NonNull<Header>) {
    let local = core(h);
    let state = local.state.get();
    if state & FINISHED != 0 {
        unsafe { (header(h).vtable.dealloc)(h) };
        return;
    }
    // nothing can wake the task, the runner drops the future
    local.refs.set(1);
    local.state.set(state | SCHEDULED | CLOSE);
    owner_queue(h).schedule_local(Runnable(h));
}

/// Drops the future on the owner thread, completes the task and releases
/// a reference.
fn close(h: NonNull<Header>) {
    // completes the task even if the drop of the future panics
    struct Finish(NonNull<Header>);

    impl Drop for Finish {
        /// Publishes cancellation, releases the close reference, and notifies the join.
        fn drop(&mut self) {
            let local = core(self.0);
            finish(self.0, local.state.get() & !(RUNNING | SCHEDULED | CLOSE));
            // the future is dropped, there is no output
            let completed = complete(self.0);
            release_local(self.0);
            completed.notify();
        }
    }

    // the destructor of the future may wake the task and poll the runtime,
    // a running task is only marked as scheduled
    let local = core(h);
    local.state.set(local.state.get() | RUNNING);
    let finish = Finish(h);
    unsafe { (header(finish.0).vtable.drop_future)(finish.0) };
}

/// Sets `state` with `FINISHED` on the owner thread, the task leaves the task
/// list.
fn finish(h: NonNull<Header>, state: u8) {
    core(h).state.set((state | FINISHED) & !LISTED);
    if state & LISTED != 0 {
        owner_queue(h).tasks.remove(h);
    }
}

/// Completion of a task, the awaiter is notified once the caller releases
/// its reference, so a panicking waker does not skip the cleanup.
///
/// Also notifies on drop, while unwinding from a panic of the future or of
/// the output drop.
struct Completed {
    /// The join handle is dropped, the output has no owner.
    detached: bool,
    awaiter: Option<crate::LocalWaker>,
}

impl Completed {
    /// Wakes the awaiter, the awaiter of a dropped join handle is dropped.
    fn notify(self) {
        drop(self);
    }
}

impl Drop for Completed {
    /// Wakes an attached join's awaiter or releases a detached join's stale waker.
    fn drop(&mut self) {
        let Some(w) = self.awaiter.take() else {
            return;
        };
        // a panic of the waker while unwinding aborts the process
        if !self.detached {
            w.wake();
        }
    }
}

/// Publishes completion and takes the awaiter, on the owner thread.
fn complete(h: NonNull<Header>) -> Completed {
    Completed {
        detached: core(h).state.get() & DETACHED != 0,
        awaiter: unsafe { header(h).awaiter.take() },
    }
}

/// Releases a reference after completion was published, also while
/// unwinding from an output drop.
struct Release(NonNull<Header>);

impl Drop for Release {
    /// Releases the caller's reference even if dropping its output panics.
    fn drop(&mut self) {
        release_local(self.0);
    }
}

/// Drops the output after completion is published, then releases a
/// reference of the caller.
fn drop_output(h: NonNull<Header>) {
    let release = Release(h);
    unsafe { (header(release.0).vtable.drop_output)(release.0) };
}

/// Spawns a task and schedules it on its runtime.
///
/// A task spawned after the runtime is dropped is cancelled, its future is
/// dropped on the calling thread.
///
/// # Safety
///
/// The caller must be on the runtime thread and ensure the captured lifetime
/// is long enough.
pub(crate) unsafe fn spawn<F: Future>(
    future: F,
    queue: &Arc<RunnableQueue>,
    location: SpawnLocation,
) -> Task<F::Output> {
    match unsafe { create::<_, true>(future, queue, location, false) } {
        Ok(h) => Task { h, _t: PhantomData },
        Err(future) => {
            let h = alloc::<F>(Stage::Consumed, queue, location, FINISHED, 1);
            let task = Task { h, _t: PhantomData };
            cancel_closed(future, location);
            task
        }
    }
}

/// Spawns a task without a join handle, the output is dropped on completion.
///
/// # Safety
///
/// The captured lifetime must be long enough. If `LOCAL` is true the caller
/// must be on the runtime thread; otherwise the future must be `Send`.
pub(crate) unsafe fn spawn_detached<F: Future, const LOCAL: bool>(
    future: F,
    queue: &Arc<RunnableQueue>,
    location: SpawnLocation,
) {
    if let Err(future) = unsafe { create::<_, LOCAL>(future, queue, location, true) } {
        cancel_closed(future, location);
    }
}

/// Drops the future of a task spawned after its runtime is dropped, the
/// task would never run.
#[cold]
fn cancel_closed<F>(future: F, location: SpawnLocation) {
    log::warn!("Task spawned at {location} after its runtime is dropped, the task is cancelled");
    drop(future);
}

/// Allocates a task and queues it on its runtime, returns the future if the
/// runtime is closed.
unsafe fn create<F: Future, const LOCAL: bool>(
    future: F,
    queue: &Arc<RunnableQueue>,
    location: SpawnLocation,
    detached: bool,
) -> Result<NonNull<Header>, F> {
    let local = LOCAL || Some(queue.id) == try_current_thread_id();
    // a remote push reserves its queue entry before the task exists
    let permit = if local {
        if queue.is_closed() {
            return Err(future);
        }
        None
    } else {
        let Some(permit) = queue.admit(false) else {
            return Err(future);
        };
        Some(permit)
    };
    let h = alloc(
        Stage::Running(future),
        queue,
        location,
        SCHEDULED | if local { LISTED } else { 0 } | if detached { DETACHED } else { 0 },
        // the queue and the join handle
        if detached { 1 } else { 2 },
    );
    // Own the join reference before scheduling can unwind through a notifier.
    let join = (!detached).then(|| Task::<F::Output> { h, _t: PhantomData });
    // the owner thread may free the task once it is queued, the queue of the
    // caller outlives the call
    if local {
        queue.tasks.push(h);
        queue.schedule_local(Runnable(h));
    } else {
        permit.unwrap().schedule(Remote(h));
    }
    mem::forget(join);
    Ok(h)
}

/// Allocates a task with owner-thread flags and reference count.
fn alloc<F: Future>(
    stage: Stage<F>,
    queue: &Arc<RunnableQueue>,
    location: SpawnLocation,
    flags: u8,
    refs: u32,
) -> NonNull<Header> {
    let raw = Box::new(TaskCore {
        core: Core {
            header: Header {
                vtable: &Fns::<F>::VTABLE,
                queue: NonNull::from(queue.as_ref()),
                location,
                awaiter: Awaiter::new(),
            },
            next: Cell::new(ptr::null_mut()),
            refs: Cell::new(refs),
            state: Cell::new(flags),
            remote_touched: AtomicBool::new(false),
            routing: wake::Routing {
                runtime: queue.runtime_id,
                owner: queue.id,
            },
            self_wakes: Cell::new(0),
            streak_start: Cell::new(0),
            list_prev: Cell::new(ptr::null_mut()),
            list_next: Cell::new(ptr::null_mut()),
            _pin: PhantomPinned,
        },
        stage: SharedCell::new(stage),
        #[cfg(test)]
        _allocation: queue.task_allocations.clone(),
    });
    // Transfer the stable allocation to intrusive ownership before it is polled.
    // The vtable drops the future in place and reclaims the allocation.
    NonNull::from(Box::leak(raw)).cast()
}

/// An owner-thread-only task in the local queue, owns a reference.
pub(crate) struct Runnable(NonNull<Header>);

/// Returns the self-wake bookkeeping of a task.
pub(crate) fn task_ref(task: &Runnable) -> TaskRef {
    TaskRef(task.0)
}

impl Runnable {
    /// Runs the task, returns `true` if the task woke itself while polled.
    pub(crate) fn run(self) -> bool {
        struct Guard(NonNull<Header>);

        impl Drop for Guard {
            /// Cancels and completes the task when its poll unwinds.
            fn drop(&mut self) {
                // the poll panicked
                let local = core(self.0);
                local
                    .state
                    .set(local.state.get() & !(RUNNING | SCHEDULED | CLOSE));
                close(self.0);
            }
        }

        // the future may be `!Send`
        debug_assert!(is_owner(self.0), "task polled off its runtime thread");
        let h = ManuallyDrop::new(self).0;
        let hdr = header(h);
        let local = core(h);

        let state = local.state.get() & !SCHEDULED;
        if state & (CLOSE | FINISHED) != 0 {
            discard(h, state);
            return false;
        }
        local.state.set(state | RUNNING);

        // The run reference protects the borrowed poll waker. Its final release
        // waits behind any acquisitions published by another thread.
        let waker = ManuallyDrop::new(unsafe { Waker::from_raw(raw_waker(h.as_ptr().cast())) });
        let mut cx = Context::from_waker(&waker);
        // a panic of the poll drops the future and completes the task
        let guard = Guard(h);
        let ready = unsafe { (hdr.vtable.poll)(h, &mut cx) };
        mem::forget(guard);

        let state = local.state.get() & !RUNNING;
        // `SCHEDULED` here is a wake during the poll
        if ready {
            finish(h, state & !(SCHEDULED | CLOSE));
            let completed = complete(h);
            // nothing takes the output of a detached task
            if completed.detached {
                drop_output(h);
            } else {
                release_local(h);
            }
            completed.notify();
            return false;
        } else if state & CLOSE != 0 {
            discard(h, state);
            return false;
        }
        local.state.set(state);
        if state & SCHEDULED != 0 {
            // the run reference passes to the queue
            owner_queue(h).schedule_local(Runnable(h));
            return true;
        }
        release_local(h);
        false
    }
}

impl Drop for Runnable {
    /// Discards the run reference on the owner, or reports an off-owner leak.
    fn drop(&mut self) {
        if is_owner(self.0) {
            discard(self.0, core(self.0).state.get());
        } else {
            leaked(self.0);
        }
    }
}

/// Unqueues a task without polling it on the owner thread, drops the future
/// of an unfinished task and releases the run reference.
fn discard(h: NonNull<Header>, state: u8) {
    core(h).state.set(state & !SCHEDULED);
    if state & FINISHED == 0 {
        close(h);
    } else {
        release_local(h);
    }
}

/// A task dropped off its runtime thread leaks, its future may be `!Send`.
#[cold]
fn leaked(h: NonNull<Header>) {
    log::error!(
        "Task spawned at {} is dropped off its runtime thread, the task is leaked",
        header(h).location
    );
}

impl fmt::Debug for Runnable {
    /// Formats the runnable's spawn location while its task reference is held.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runnable")
            .field("location", &header(self.0).location)
            .finish()
    }
}

/// An unpolled, detached `Send` future submitted to its owner, owns a reference.
pub(crate) struct Remote(NonNull<Header>);

unsafe impl Send for Remote {}

impl Remote {
    /// Activates a submission on the owner thread.
    pub(crate) fn into_runnable(self) -> Runnable {
        debug_assert!(is_owner(self.0), "task merged off its runtime thread");
        let h = ManuallyDrop::new(self).0;
        let local = core(h);
        debug_assert_eq!(local.state.get(), SCHEDULED | DETACHED);
        local.state.set(local.state.get() | LISTED);
        owner_queue(h).tasks.push(h);
        Runnable(h)
    }
}

impl Drop for Remote {
    /// Cancels an accepted submission on the owner, or reports an off-owner leak.
    fn drop(&mut self) {
        if is_owner(self.0) {
            // Activate the accepted submission, then drop its future unpolled.
            drop(Remote(self.0).into_runnable());
        } else {
            leaked(self.0);
        }
    }
}

impl fmt::Debug for Remote {
    /// Identifies a remote submission without accessing its owner-only task state.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote").finish()
    }
}

/// Self-wake bookkeeping of a task, valid while the task is alive.
#[derive(Clone, Copy)]
pub(crate) struct TaskRef(NonNull<Header>);

impl TaskStats for TaskRef {
    /// Returns the spawn location while the task allocation remains alive.
    fn location(self) -> SpawnLocation {
        header(self.0).location
    }

    /// Returns the self-wake count with its reported marker.
    fn self_wakes(self) -> u32 {
        let local = core(self.0);
        let reported = if local.state.get() & WAKES_REPORTED != 0 {
            REPORTED
        } else {
            0
        };
        u32::from(local.self_wakes.get()) | reported
    }

    /// Stores a bounded self-wake count and reported marker without changing other flags.
    fn set_self_wakes(self, wakes: u32) {
        let local = core(self.0);
        let count = (wakes & !REPORTED).min(MAX_SELF_WAKES);
        let reported = if wakes & REPORTED != 0 { WAKES_REPORTED } else { 0 };
        local
            .state
            .set((local.state.get() & !WAKES_REPORTED) | reported);
        local.self_wakes.set(count as u16);
    }

    /// Returns the owner's cached start timestamp for the current self-wake streak.
    fn streak_start(self) -> u32 {
        core(self.0).streak_start.get()
    }

    /// Stores the streak's start timestamp on the owner thread.
    fn set_streak_start(self, start: u32) {
        core(self.0).streak_start.set(start);
    }
}

/// Intrusive FIFO of runnable tasks, used on the owner thread only.
pub(crate) struct LocalQueue {
    head: Cell<*mut Header>,
    tail: Cell<*mut Header>,
}

impl LocalQueue {
    /// Creates an empty owner-thread runnable FIFO.
    pub(crate) const fn new() -> Self {
        Self {
            head: Cell::new(ptr::null_mut()),
            tail: Cell::new(ptr::null_mut()),
        }
    }

    /// Appends a task, the queue takes its reference.
    pub(crate) fn push(&self, task: Runnable) {
        let h = ManuallyDrop::new(task).0;
        core(h).next.set(ptr::null_mut());
        if let Some(tail) = NonNull::new(self.tail.replace(h.as_ptr())) {
            core(tail).next.set(h.as_ptr());
        } else {
            self.head.set(h.as_ptr());
        }
    }

    /// Pops the first task, its reference passes to the caller.
    pub(crate) fn pop(&self) -> Option<Runnable> {
        let h = NonNull::new(self.head.get())?;
        let next = core(h).next.get();
        self.head.set(next);
        if next.is_null() {
            self.tail.set(ptr::null_mut());
        }
        Some(Runnable(h))
    }

    /// Returns whether the owner-thread FIFO contains no runnables.
    pub(crate) fn is_empty(&self) -> bool {
        self.head.get().is_null()
    }
}

impl fmt::Debug for LocalQueue {
    /// Identifies the queue without traversing its intrusive task links.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalQueue").finish_non_exhaustive()
    }
}

/// Intrusive list of unfinished tasks, used on the owner thread.
///
/// A task waiting on a waker is in no queue, the runtime drops its future
/// through the list when it is dropped. The list holds no references.
pub(crate) struct TaskList {
    head: Cell<*mut Header>,
}

impl TaskList {
    /// Creates an empty, non-owning list of unfinished tasks.
    pub(crate) const fn new() -> Self {
        Self {
            head: Cell::new(ptr::null_mut()),
        }
    }

    /// Inserts an unlisted task at the head.
    fn push(&self, h: NonNull<Header>) {
        let local = core(h);
        let head = self.head.replace(h.as_ptr());
        local.list_prev.set(ptr::null_mut());
        local.list_next.set(head);
        if let Some(head) = NonNull::new(head) {
            core(head).list_prev.set(h.as_ptr());
        }
    }

    /// Unlinks a listed task, the caller clears `LISTED`.
    fn remove(&self, h: NonNull<Header>) {
        let local = core(h);
        let (prev, next) = (local.list_prev.get(), local.list_next.get());
        if let Some(prev) = NonNull::new(prev) {
            core(prev).list_next.set(next);
        } else {
            self.head.set(next);
        }
        if let Some(next) = NonNull::new(next) {
            core(next).list_prev.set(prev);
        }
    }

    /// Drops the future of the first task, returns `false` if the list is
    /// empty.
    pub(crate) fn close_first(&self) -> bool {
        let Some(h) = NonNull::new(self.head.get()) else {
            return false;
        };
        let local = core(h);
        let state = local.state.get();
        if state & RUNNING != 0 {
            // Retire now, but let its active runner drop the pinned future.
            // A pending self-wake must not reschedule it after shutdown.
            local.state.set((state | CLOSE) & !LISTED);
            self.remove(h);
        } else {
            // a reference for `close`, a queued task is finished when popped
            inc_local(h);
            close(h);
        }
        true
    }
}

impl fmt::Debug for TaskList {
    /// Identifies the task list without traversing its non-owning links.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskList").finish_non_exhaustive()
    }
}

/// Join side of a task, owns a reference.
pub(crate) struct Task<T> {
    h: NonNull<Header>,
    _t: PhantomData<T>,
}

impl<T> Unpin for Task<T> {}

impl<T> Task<T> {
    /// Returns `true` once the future is dropped, with or without an output.
    pub(crate) fn is_finished(&self) -> bool {
        core(self.h).state.get() & FINISHED != 0
    }

    /// Cancels the task and drops the join handle, the owner thread drops
    /// the future. The output of a finished task is dropped.
    pub(crate) fn cancel(self) {
        let h = ManuallyDrop::new(self).h;
        detach_local(h, true);
    }

    /// Polls the task output, `None` if the task was cancelled or panicked.
    pub(crate) fn poll_join(&mut self, cx: &Context<'_>) -> Poll<Option<T>> {
        let hdr = header(self.h);
        let local = core(self.h);
        if local.state.get() & FINISHED == 0 {
            // The waker callbacks may poll the runtime and complete the task.
            drop(unsafe { hdr.awaiter.register(cx.waker()) });
            if local.state.get() & FINISHED == 0 {
                return Poll::Pending;
            }
            drop(unsafe { hdr.awaiter.take() });
        }
        Poll::Ready(self.take_output())
    }

    /// Takes the output, `None` if the task produced none.
    fn take_output(&mut self) -> Option<T> {
        let mut out = None;
        unsafe { (header(self.h).vtable.take_output)(self.h, (&raw mut out).cast()) };
        out
    }
}

impl<T> Drop for Task<T> {
    /// Detaches the local join and releases its reference without requesting cancellation.
    fn drop(&mut self) {
        detach_local(self.h, false);
    }
}

/// Drops a join handle on the owner thread, `cancel` also drops the future.
fn detach_local(h: NonNull<Header>, cancel: bool) {
    let hdr = header(h);
    let local = core(h);
    let state = local.state.get();
    if state & FINISHED != 0 {
        drop_output(h);
        return;
    }
    // nothing waits for the output, the waker is dropped once the task is
    // consistent, its drop may run user code
    let awaiter = unsafe { hdr.awaiter.take() };
    // the runner of a cancelled task drops the future, a running task may
    // still complete
    let state = state | DETACHED | if cancel { CLOSE } else { 0 };
    if cancel && state & (SCHEDULED | RUNNING) == 0 {
        local.state.set(state | SCHEDULED);
        // the handle reference passes to the queue
        owner_queue(h).schedule_local(Runnable(h));
    } else {
        local.state.set(state);
        release_local(h);
    }
    drop(awaiter);
}

impl<T> fmt::Debug for Task<T> {
    /// Formats the joined task's spawn location while its reference is held.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("location", &header(self.h).location)
            .finish()
    }
}

#[cfg(all(test, not(ntex_loom)))]
#[path = "task_core_tests.rs"]
mod core_tests;

#[cfg(all(test, ntex_loom))]
#[test]
#[should_panic(expected = "currently writing to cell")]
fn task_loom_shared_cell_access() {
    use loom::sync::atomic::Ordering;

    struct Shared(SharedCell<usize>);

    // Deliberately allow conflicting accesses to verify the tracking scope.
    unsafe impl Sync for Shared {}

    loom::model(|| {
        let cell = Arc::new(Shared(SharedCell::new(0)));
        let entered = Arc::new(loom::sync::atomic::AtomicBool::new(false));
        let (other, entered2) = (cell.clone(), entered.clone());
        let writer = loom::thread::spawn(move || {
            other.0.with_mut(|p| {
                entered2.store(true, Ordering::Release);
                loom::thread::yield_now();
                unsafe { *p = 1 };
            });
        });
        while !entered.load(Ordering::Acquire) {
            loom::thread::yield_now();
        }
        cell.0.with_mut(|p| unsafe { *p = 2 });
        writer.join().unwrap();
    });
}
