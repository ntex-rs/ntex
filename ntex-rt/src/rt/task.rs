//! Task type of the neon runtime.
//!
//! A task is polled only on the thread of its runtime, the owner thread.
//! Wakers and the join handle may be used on any thread.
//!
//! The owner thread keeps the task state and the reference count in plain
//! cells. Other threads update the `remote` word: flags plus a signed
//! reference count delta. A remote thread that needs the owner thread to act,
//! wake or cancel the task, or release a reference, queues the task on the
//! runtime remote queue once (`R_QUEUED`), the owner thread merges the remote
//! word when it pops the task. A running task is freed on the owner thread
//! only, once the local count and the delta are both zero.
//!
//! On completion the owner thread moves the local count to the remote word.
//! The future and the output of a finished task are dropped, the last
//! reference frees it on any thread, also after the runtime is dropped.
//!
//! The runtime queues are intrusive, a task links itself into the local
//! queue and into the remote queue. The runtime also lists its unfinished
//! tasks, it drops their futures when it is dropped.
use std::cell::Cell;
use std::sync::{Arc, atomic::Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::{fmt, future::Future, marker::PhantomData, mem, mem::ManuallyDrop, pin::Pin};
use std::{process::abort, ptr, ptr::NonNull};

use crate::handle::SpawnLocation;
use crate::rt::{REPORTED, RunnableQueue, TaskStats, current_thread_id};

#[cfg(not(ntex_loom))]
use std::sync::atomic::{AtomicPtr, AtomicU32};

#[cfg(ntex_loom)]
use loom::sync::atomic::{AtomicPtr, AtomicU32};

/// A cell shared between threads, loom tracks its accesses under `ntex_loom`.
struct SharedCell<T>(
    #[cfg(not(ntex_loom))] std::cell::UnsafeCell<T>,
    #[cfg(ntex_loom)] loom::cell::UnsafeCell<T>,
);

impl<T> SharedCell<T> {
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
const SCHEDULED: u32 = 1;
const RUNNING: u32 = 1 << 1;
/// The future is dropped.
const FINISHED: u32 = 1 << 2;
/// Cancel requested, the runner drops the future.
const CLOSE: u32 = 1 << 3;
/// The self-wake streak is reported.
const WAKES_REPORTED: u32 = 1 << 4;
/// No join handle, the output is dropped on completion.
const DETACHED: u32 = 1 << 5;
/// In the task list of the runtime.
const LISTED: u32 = 1 << 6;
/// The state keeps the self-wake count above the flags.
const WAKES_SHIFT: u32 = 8;
/// The self-wake count saturates at this value.
pub(crate) const MAX_SELF_WAKES: u32 = u32::MAX >> WAKES_SHIFT;

// remote word
/// Queued in the remote queue, the queue holds a reference.
const R_QUEUED: u32 = 1;
/// Woken from another thread.
const R_WOKEN: u32 = 1 << 1;
/// Cancelled from another thread.
const R_CANCEL: u32 = 1 << 2;
/// The future is dropped, set by the owner thread.
const R_FINISHED: u32 = 1 << 3;
/// A join handle on another thread is writing the awaiter.
const R_REGISTERING: u32 = 1 << 4;
/// The join handle is dropped on another thread before completion.
const R_DETACHED: u32 = 1 << 5;
/// The reference count delta is stored above the flags.
const R_SHIFT: u32 = 6;
/// One reference of the delta.
const R_REF: u32 = 1 << R_SHIFT;
/// Mask of the remote flags.
const R_FLAGS: u32 = R_REF - 1;
/// Half of the delta range, racing increments stay in range until one of
/// them aborts.
const MAX_REFS: i32 = i32::MAX >> (R_SHIFT + 1);

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

/// Link of the remote queue.
struct Link {
    next: AtomicPtr<Link>,
}

#[repr(C)]
struct Header {
    /// Remote queue link, the first field, a link pointer is a header pointer.
    link: Link,
    /// Local queue link.
    next: Cell<*mut Header>,
    /// Task list links.
    list_prev: Cell<*mut Header>,
    list_next: Cell<*mut Header>,
    vtable: &'static VTable,
    /// Queues of the runtime, its id is the owner thread.
    queue: Arc<RunnableQueue>,
    /// Where the task is spawned, for logs and stats.
    location: SpawnLocation,
    /// Waker of the join handle.
    awaiter: Awaiter,
    /// `R_*` flags and the reference count delta of other threads.
    remote: AtomicU32,
    /// Flags and the self-wake count.
    state: Cell<u32>,
    /// Local reference count, moved to the remote word on completion.
    refs: Cell<u32>,
    /// Start of the self-wake streak, milliseconds since the runtime epoch.
    streak_start: Cell<u32>,
}

#[cfg(all(target_pointer_width = "64", not(ntex_loom)))]
const _: () = assert!(mem::size_of::<Header>() == 80);

enum Stage<F: Future> {
    /// The future, pinned in place.
    Running(F),
    /// The output, until the join handle takes or drops it.
    Finished(F::Output),
    /// The future is dropped, and the output is taken, dropped or never
    /// produced.
    Consumed,
}

/// Allocation of a task, the header is the first field, a header pointer is
/// a task pointer.
#[repr(C)]
struct RawTask<F: Future> {
    header: Header,
    stage: SharedCell<Stage<F>>,
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
        unsafe { (*h.cast::<RawTask<F>>().as_ptr()).stage.with_mut(f) }
    }

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

    unsafe fn drop_future(h: NonNull<Header>) {
        // the future is pinned, it is dropped in place
        struct Guard<F: Future>(*mut Stage<F>);

        impl<F: Future> Drop for Guard<F> {
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

    unsafe fn take_output(h: NonNull<Header>, out: *mut ()) {
        if let Some(v) = unsafe { Self::take_output_value(h) } {
            unsafe { *out.cast::<Option<F::Output>>() = Some(v) };
        }
    }

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
        drop(unsafe { Box::from_raw(h.cast::<RawTask<F>>().as_ptr()) });
    }
}

/// Reference count delta of the remote word.
#[inline]
fn delta(word: u32) -> i32 {
    word.cast_signed() >> R_SHIFT
}

/// Waker of the join handle.
///
/// Null, an ntex task waker as its task pointer, or a boxed waker tagged
/// with `BOXED`.
struct Awaiter(SharedCell<*mut ()>);

const BOXED: usize = 1;

impl Awaiter {
    fn new() -> Self {
        Self(SharedCell::new(ptr::null_mut()))
    }

    /// Takes the stored waker.
    ///
    /// # Safety
    ///
    /// The caller should have exclusive access to the awaiter.
    unsafe fn take(&self) -> Option<Waker> {
        let p = self.0.with_mut(|p| unsafe { p.replace(ptr::null_mut()) });
        if p.is_null() {
            None
        } else if p.addr() & BOXED != 0 {
            let w = unsafe { Box::from_raw(p.map_addr(|a| a & !BOXED).cast::<Waker>()) };
            Some(*w)
        } else {
            Some(unsafe { Waker::from_raw(RawWaker::new(p, &WAKER_VTABLE)) })
        }
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
    unsafe fn register(&self, waker: &Waker) -> Option<Waker> {
        let cur = self.0.with_mut(|p| unsafe { *p });
        // a waker that wakes the stored waker is kept
        let new = if ptr::eq(waker.vtable(), &raw const WAKER_VTABLE) {
            if ptr::eq(cur, waker.data()) {
                return None;
            }
            // the stored pointer owns the cloned reference
            ManuallyDrop::new(waker.clone()).data().cast_mut()
        } else {
            if cur.addr() & BOXED != 0
                && unsafe { &*cur.map_addr(|a| a & !BOXED).cast::<Waker>() }.will_wake(waker)
            {
                return None;
            }
            Box::into_raw(Box::new(waker.clone()))
                .cast::<()>()
                .map_addr(|a| a | BOXED)
        };
        // the clone may have completed the task and taken the awaiter
        let old = unsafe { self.take() };
        self.0.with_mut(|p| unsafe { *p = new });
        old
    }
}

impl Drop for Awaiter {
    fn drop(&mut self) {
        drop(unsafe { self.take() });
    }
}

/// Waker of a task, the data pointer is the header and owns a reference.
static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

/// Recognizes the supplied native waker on its owner thread.
#[inline]
pub(crate) fn is_local_waker(waker: &Waker) -> bool {
    ptr::eq(waker.vtable(), &raw const WAKER_VTABLE) && is_owner(unsafe { task_ptr(waker.data()) })
}

/// Clones a native reference on its owner thread.
///
/// The caller must hold a native task reference and be on its owner thread.
#[inline]
pub(crate) unsafe fn clone_local_waker(ptr: *const ()) {
    let h = unsafe { task_ptr(ptr) };
    if header(h).state.get() & FINISHED == 0 {
        inc_local(h);
    } else {
        inc_shared(h);
    }
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

/// Uses the thread-safe vtable, including when converting a local reference.
#[inline]
pub(crate) fn raw_waker(ptr: *const ()) -> RawWaker {
    RawWaker::new(ptr, &WAKER_VTABLE)
}

/// Header of a task, the caller holds a reference for the lifetime.
#[inline]
fn header<'a>(h: NonNull<Header>) -> &'a Header {
    unsafe { h.as_ref() }
}

/// Returns `true` on the owner thread of the task.
#[inline]
fn is_owner(h: NonNull<Header>) -> bool {
    header(h).queue.id == current_thread_id()
}

/// Task of a waker data pointer.
#[inline]
unsafe fn task_ptr(ptr: *const ()) -> NonNull<Header> {
    unsafe { NonNull::new_unchecked(ptr.cast_mut().cast::<Header>()) }
}

/// Adds a reference, a local one on the owner thread before completion,
/// otherwise a remote one.
unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    let h = unsafe { task_ptr(ptr) };
    if is_owner(h) {
        unsafe { clone_local_waker(ptr) };
    } else {
        inc_shared(h);
    }
    raw_waker(ptr)
}

fn inc_shared(h: NonNull<Header>) {
    let prev = header(h).remote.fetch_add(R_REF, Ordering::Relaxed);
    if delta(prev) > MAX_REFS {
        abort();
    }
}

unsafe fn wake(ptr: *const ()) {
    wake_any(unsafe { task_ptr(ptr) }, true);
}

unsafe fn wake_by_ref(ptr: *const ()) {
    wake_any(unsafe { task_ptr(ptr) }, false);
}

unsafe fn drop_waker(ptr: *const ()) {
    release(unsafe { task_ptr(ptr) });
}

/// Wakes the task on any thread, `owned` passes a reference.
fn wake_any(h: NonNull<Header>, owned: bool) {
    if is_owner(h) {
        wake_local(h, owned);
    } else {
        remote_signal(h, R_WOKEN, owned);
    }
}

/// Adds a local reference on the owner thread, aborts on overflow.
fn inc_local(h: NonNull<Header>) {
    let refs = &header(h).refs;
    match refs.get().checked_add(1) {
        Some(n) => refs.set(n),
        None => abort(),
    }
}

/// Releases a reference on any thread.
fn release(h: NonNull<Header>) {
    if is_owner(h) {
        release_local(h);
    } else {
        remote_signal(h, 0, true);
    }
}

/// Wakes the task on the owner thread, `owned` passes a reference.
fn wake_local(h: NonNull<Header>, owned: bool) {
    let hdr = header(h);
    let state = hdr.state.get();
    if state & (SCHEDULED | FINISHED) == 0 {
        hdr.state.set(state | SCHEDULED);
        // a running task is queued by the runner after the poll
        if state & RUNNING == 0 {
            if !owned {
                inc_local(h);
            }
            hdr.queue.schedule_local(Runnable(h));
            return;
        }
    }
    if owned {
        release_local(h);
    }
}

/// Sets `flags` in the remote word from another thread, and queues the task
/// unless it is queued. `owned` passes a reference.
fn remote_signal(h: NonNull<Header>, flags: u32, owned: bool) {
    let remote = &header(h).remote;
    let mut permit = None;
    let mut cur = remote.load(Ordering::Relaxed);
    loop {
        if cur & R_FINISHED != 0 {
            // nothing to run, the owner thread may be gone
            drop(permit);
            if owned {
                release_shared(h);
            }
            return;
        }
        if delta(cur).abs() > MAX_REFS {
            abort();
        }
        let set = flags & !cur;
        // a wake that finds its flags set still writes the word, a plain
        // return would not publish the writes before the wake, and the owner
        // may have taken the pending wake since the load
        let (new, push) = if cur & R_QUEUED != 0 {
            if owned {
                ((cur | set).wrapping_sub(R_REF), false)
            } else {
                (cur | set, false)
            }
        } else if owned && set == 0 && delta(cur) > 0 {
            // a live task has local references outside of `last_ref`, which
            // re-reads the delta, so this is not the last reference
            (cur.wrapping_sub(R_REF), false)
        } else if owned {
            // the reference passes to the queue. A plain drop may be the last
            // reference, only the owner thread can tell. Such drops are rare,
            // wakers sent to other threads end with a wake, so the queue push
            // and notify of a drop are kept
            (cur | set | R_QUEUED, true)
        } else if set != 0 {
            ((cur | set | R_QUEUED).wrapping_add(R_REF), true)
        } else {
            (cur, false)
        };
        if push && permit.is_none() {
            permit = header(h).queue.admit(owned);
            if permit.is_none() {
                // Shutdown owns cleanup of unfinished tasks. Release the
                // caller's reference without reserving an unconsumable entry.
                if owned {
                    let prev = remote.fetch_sub(R_REF, Ordering::AcqRel);
                    if prev & R_FINISHED != 0 && delta(prev) == 1 {
                        unsafe { (header(h).vtable.dealloc)(h) };
                    }
                }
                return;
            }
        }
        match remote.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) if push => break,
            Ok(_) => return,
            Err(v) => cur = v,
        }
    }
    permit.unwrap().schedule(Remote(h));
}

/// Releases a reference on the owner thread.
fn release_local(h: NonNull<Header>) {
    let hdr = header(h);
    if hdr.state.get() & FINISHED != 0 {
        release_shared(h);
        return;
    }
    let refs = hdr.refs.get() - 1;
    hdr.refs.set(refs);
    if refs == 0 {
        last_ref(h);
    }
}

/// The local count of an unfinished task dropped to zero, on the owner
/// thread.
///
/// The references of other threads become the local count. Without them
/// nothing can wake the task, its future is dropped.
#[cold]
fn last_ref(h: NonNull<Header>) {
    let hdr = header(h);
    // references cloned on other threads are counted in the remote delta
    let mut cur = hdr.remote.load(Ordering::Acquire);
    loop {
        let delta = delta(cur);
        if delta == 0 {
            break;
        }
        // the flags stay, the delta moves to the local count
        match hdr.remote.compare_exchange_weak(
            cur,
            cur & R_FLAGS,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // the local count is zero, the delta is the reference count
                hdr.refs
                    .set(u32::try_from(delta).unwrap_or_else(|_| abort()));
                return;
            }
            Err(v) => cur = v,
        }
    }

    // a finished task is released by `release_shared`
    let state = hdr.state.get();
    debug_assert_eq!(state & FINISHED, 0);
    // nothing can wake the task, the runner drops the future
    hdr.refs.set(1);
    hdr.state.set(state | SCHEDULED | CLOSE);
    hdr.queue.schedule_local(Runnable(h));
}

/// Releases a reference of a finished task on any thread.
///
/// The remote word counts the references of a finished task, the last
/// reference frees it on the thread that releases it, the owner thread may
/// be gone. The future and the output are dropped, the rest is `Send`.
fn release_shared(h: NonNull<Header>) {
    let prev = header(h).remote.fetch_sub(R_REF, Ordering::AcqRel);
    if delta(prev) == 1 {
        unsafe { (header(h).vtable.dealloc)(h) };
    }
}

/// Drops the future on the owner thread, completes the task and releases
/// a reference.
fn close(h: NonNull<Header>) {
    // completes the task even if the drop of the future panics
    struct Finish(NonNull<Header>);

    impl Drop for Finish {
        fn drop(&mut self) {
            let hdr = header(self.0);
            finish(self.0, hdr.state.get() & !(RUNNING | SCHEDULED | CLOSE));
            // the future is dropped, there is no output
            let completed = complete(self.0);
            release_local(self.0);
            completed.notify();
        }
    }

    // the destructor of the future may wake the task and poll the runtime,
    // a running task is only marked as scheduled
    let hdr = header(h);
    hdr.state.set(hdr.state.get() | RUNNING);
    let finish = Finish(h);
    unsafe { (header(finish.0).vtable.drop_future)(finish.0) };
}

/// Sets `state` with `FINISHED` on the owner thread, the task leaves the task
/// list.
fn finish(h: NonNull<Header>, state: u32) {
    let hdr = header(h);
    hdr.state.set((state | FINISHED) & !LISTED);
    if state & LISTED != 0 {
        hdr.queue.tasks.remove(h);
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
    awaiter: Option<Waker>,
}

impl Completed {
    /// Wakes the awaiter, the awaiter of a dropped join handle is dropped.
    fn notify(self) {
        drop(self);
    }
}

impl Drop for Completed {
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
    let hdr = header(h);
    // the local count moves to the remote word, before any waker callback
    let refs = hdr.refs.replace(0);
    if refs > MAX_REFS.cast_unsigned() {
        abort();
    }
    // `R_FINISHED` is not set, the addition sets it
    let prev = hdr
        .remote
        .fetch_add((refs << R_SHIFT) | R_FINISHED, Ordering::AcqRel);
    // a join handle dropped on another thread leaves its awaiter
    let detached = hdr.state.get() & DETACHED != 0 || prev & R_DETACHED != 0;
    // a registering join handle sees `R_FINISHED` and handles the awaiter
    let awaiter = if prev & R_REGISTERING == 0 {
        unsafe { hdr.awaiter.take() }
    } else {
        None
    };
    Completed { detached, awaiter }
}

#[cfg(test)]
thread_local! {
    /// Completions through `finish_unshared` on this thread.
    pub(crate) static UNSHARED_FINISHES: Cell<usize> = const { Cell::new(0) };
}

/// Completes a task that has no references but the run reference, on the
/// owner thread. Drops the output and frees the task.
///
/// No join handle, waker or queue refers to the task, nothing can observe
/// or reference it, so the completion is not published.
fn finish_unshared(h: NonNull<Header>) {
    // frees the task even if a waker callback or the output drop panics
    struct Dealloc(NonNull<Header>);

    impl Drop for Dealloc {
        fn drop(&mut self) {
            unsafe { (header(self.0).vtable.dealloc)(self.0) };
        }
    }

    #[cfg(test)]
    UNSHARED_FINISHES.with(|n| n.set(n.get() + 1));
    let task = Dealloc(h);
    let hdr = header(task.0);
    debug_assert_ne!(hdr.state.get() & DETACHED, 0);
    // the join handle is dropped, the free drops a left awaiter
    unsafe { (hdr.vtable.drop_output)(task.0) };
}

/// Clears `R_REGISTERING` if cloning the join waker unwinds. The stored
/// awaiter is unchanged, a completion meanwhile skipped it and the free
/// drops it.
struct Registering<'a>(&'a AtomicU32);

impl Drop for Registering<'_> {
    fn drop(&mut self) {
        self.0.fetch_and(!R_REGISTERING, Ordering::AcqRel);
    }
}

/// Releases a reference after completion was published, also while
/// unwinding from an output drop.
struct Release(NonNull<Header>);

impl Drop for Release {
    fn drop(&mut self) {
        let h = self.0;
        let cur = header(h).remote.load(Ordering::Acquire);
        debug_assert_ne!(cur & R_FINISHED, 0);
        if delta(cur) == 1 {
            unsafe { (header(h).vtable.dealloc)(h) };
        } else {
            release_shared(h);
        }
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
/// The caller should ensure the captured lifetime is long enough, and that
/// a future spawned on another thread is `Send`.
/// If `LOCAL` is true, the caller must be on the runtime thread.
pub(crate) unsafe fn spawn<F: Future, const LOCAL: bool>(
    future: F,
    queue: &Arc<RunnableQueue>,
    location: SpawnLocation,
) -> Task<F::Output> {
    match unsafe { create::<_, LOCAL>(future, queue, location, false) } {
        Ok(h) => Task { h, _t: PhantomData },
        Err(future) => {
            // finished without an output, the remote word counts the join handle
            let h = alloc::<F>(
                Stage::Consumed,
                queue,
                location,
                FINISHED,
                R_REF | R_FINISHED,
                0,
            );
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
/// Same as [`spawn`].
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
    let local = LOCAL || queue.id == current_thread_id();
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
        // a task spawned on another thread is listed on its first merge
        if local { SCHEDULED | LISTED } else { 0 } | if detached { DETACHED } else { 0 },
        if local { 0 } else { R_QUEUED | R_WOKEN },
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

/// Allocates a task with the owner-thread `flags`, the `remote` word and the
/// local count `refs`.
fn alloc<F: Future>(
    stage: Stage<F>,
    queue: &Arc<RunnableQueue>,
    location: SpawnLocation,
    flags: u32,
    remote: u32,
    refs: u32,
) -> NonNull<Header> {
    let raw = Box::new(RawTask {
        header: Header {
            link: Link {
                next: AtomicPtr::new(ptr::null_mut()),
            },
            next: Cell::new(ptr::null_mut()),
            list_prev: Cell::new(ptr::null_mut()),
            list_next: Cell::new(ptr::null_mut()),
            vtable: &Fns::<F>::VTABLE,
            queue: queue.clone(),
            location,
            awaiter: Awaiter::new(),
            remote: AtomicU32::new(remote),
            state: Cell::new(flags),
            refs: Cell::new(refs),
            streak_start: Cell::new(0),
        },
        stage: SharedCell::new(stage),
    });
    NonNull::from(Box::leak(raw)).cast::<Header>()
}

/// A task in the local queue, owns a reference.
pub(crate) struct Runnable(NonNull<Header>);

unsafe impl Send for Runnable {}

/// Returns the self-wake bookkeeping of a task.
pub(crate) fn task_ref(task: &Runnable) -> TaskRef {
    TaskRef(task.0)
}

impl Runnable {
    /// Runs the task, returns `true` if the task woke itself while polled.
    pub(crate) fn run(self) -> bool {
        struct Guard(NonNull<Header>);

        impl Drop for Guard {
            fn drop(&mut self) {
                // the poll panicked
                let hdr = header(self.0);
                hdr.state
                    .set(hdr.state.get() & !(RUNNING | SCHEDULED | CLOSE));
                close(self.0);
            }
        }

        // the future may be `!Send`
        debug_assert!(is_owner(self.0), "task polled off its runtime thread");
        let h = ManuallyDrop::new(self).0;
        let hdr = header(h);

        let state = hdr.state.get() & !SCHEDULED;
        if state & (CLOSE | FINISHED) != 0 {
            discard(h, state);
            return false;
        }
        hdr.state.set(state | RUNNING);

        // the waker borrows the run reference
        let waker = ManuallyDrop::new(unsafe {
            Waker::from_raw(RawWaker::new(h.as_ptr().cast(), &WAKER_VTABLE))
        });
        let mut cx = Context::from_waker(&waker);
        // a panic of the poll drops the future and completes the task
        let guard = Guard(h);
        let ready = unsafe { (hdr.vtable.poll)(h, &mut cx) };
        mem::forget(guard);

        let state = hdr.state.get() & !RUNNING;
        // `SCHEDULED` here is a wake during the poll
        if ready {
            finish(h, state & !(SCHEDULED | CLOSE));
            // only the run reference is left, nothing observes the completion
            if hdr.refs.get() == 1 && hdr.remote.load(Ordering::Acquire) == 0 {
                finish_unshared(h);
                return false;
            }
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
        hdr.state.set(state);
        if state & SCHEDULED != 0 {
            // the run reference passes to the queue
            hdr.queue.schedule_local(Runnable(h));
            return true;
        }
        release_local(h);
        false
    }
}

impl Drop for Runnable {
    fn drop(&mut self) {
        if is_owner(self.0) {
            discard(self.0, header(self.0).state.get());
        } else {
            leaked(self.0);
        }
    }
}

/// Unqueues a task without polling it on the owner thread, drops the future
/// of an unfinished task and releases the run reference.
fn discard(h: NonNull<Header>, state: u32) {
    header(h).state.set(state & !SCHEDULED);
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runnable")
            .field("location", &header(self.0).location)
            .finish()
    }
}

/// A task in the remote queue, owns a reference.
pub(crate) struct Remote(NonNull<Header>);

unsafe impl Send for Remote {}

impl Remote {
    /// Merges remote updates on the owner thread, returns the task if it
    /// should run.
    pub(crate) fn into_runnable(self) -> Option<Runnable> {
        debug_assert!(is_owner(self.0), "task merged off its runtime thread");
        let h = ManuallyDrop::new(self).0;
        let hdr = header(h);

        if hdr.state.get() & FINISHED != 0 {
            // queued before completion, the remote word counts the queue
            // reference. The flags are left set, nothing reads them once
            // `R_FINISHED` is set
            release_shared(h);
            return None;
        }
        // takes the delta and the requests, keeps the flags of the join handle
        let cur = hdr
            .remote
            .fetch_and(R_FINISHED | R_REGISTERING | R_DETACHED, Ordering::AcqRel);
        // the references of other threads join the local count
        let refs = i64::from(hdr.refs.get()) + i64::from(delta(cur));
        hdr.refs
            .set(u32::try_from(refs).unwrap_or_else(|_| abort()));

        let mut state = hdr.state.get();
        if state & LISTED == 0 {
            // the first merge of a task spawned on another thread
            state |= LISTED;
            hdr.state.set(state);
            hdr.queue.tasks.push(h);
        }
        if cur & R_CANCEL != 0 && state & FINISHED == 0 {
            state |= CLOSE;
            hdr.state.set(state);
        }
        if cur & (R_WOKEN | R_CANCEL) != 0 && state & (SCHEDULED | FINISHED) == 0 {
            hdr.state.set(state | SCHEDULED);
            if state & RUNNING == 0 {
                // the queue reference passes to the runnable
                return Some(Runnable(h));
            }
            // a running task is queued by the runner after the poll
        }
        release_local(h);
        None
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if is_owner(self.0) {
            // merges the updates, a task that should run is dropped unpolled
            drop(Remote(self.0).into_runnable());
        } else {
            leaked(self.0);
        }
    }
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote").finish()
    }
}

/// Self-wake bookkeeping of a task, valid while the task is alive.
#[derive(Clone, Copy)]
pub(crate) struct TaskRef(NonNull<Header>);

impl TaskStats for TaskRef {
    fn location(self) -> SpawnLocation {
        header(self.0).location
    }

    fn self_wakes(self) -> u32 {
        let state = header(self.0).state.get();
        let reported = if state & WAKES_REPORTED != 0 { REPORTED } else { 0 };
        (state >> WAKES_SHIFT) | reported
    }

    fn set_self_wakes(self, wakes: u32) {
        let hdr = header(self.0);
        let count = (wakes & !REPORTED).min(MAX_SELF_WAKES);
        let reported = if wakes & REPORTED != 0 { WAKES_REPORTED } else { 0 };
        let flags = hdr.state.get() & ((1 << WAKES_SHIFT) - 1) & !WAKES_REPORTED;
        hdr.state.set(flags | reported | (count << WAKES_SHIFT));
    }

    fn streak_start(self) -> u32 {
        header(self.0).streak_start.get()
    }

    fn set_streak_start(self, start: u32) {
        header(self.0).streak_start.set(start);
    }
}

/// Intrusive FIFO of runnable tasks, used on the owner thread only.
pub(crate) struct LocalQueue {
    head: Cell<*mut Header>,
    tail: Cell<*mut Header>,
}

impl LocalQueue {
    pub(crate) const fn new() -> Self {
        Self {
            head: Cell::new(ptr::null_mut()),
            tail: Cell::new(ptr::null_mut()),
        }
    }

    /// Appends a task, the queue takes its reference.
    pub(crate) fn push(&self, task: Runnable) {
        let h = ManuallyDrop::new(task).0;
        header(h).next.set(ptr::null_mut());
        let tail = self.tail.replace(h.as_ptr());
        if tail.is_null() {
            self.head.set(h.as_ptr());
        } else {
            unsafe { (*tail).next.set(h.as_ptr()) };
        }
    }

    /// Pops the first task, its reference passes to the caller.
    pub(crate) fn pop(&self) -> Option<Runnable> {
        let h = NonNull::new(self.head.get())?;
        let next = header(h).next.get();
        self.head.set(next);
        if next.is_null() {
            self.tail.set(ptr::null_mut());
        }
        Some(Runnable(h))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.head.get().is_null()
    }
}

impl fmt::Debug for LocalQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalQueue").finish_non_exhaustive()
    }
}

/// Intrusive list of the unfinished tasks of a runtime, used on the owner
/// thread only.
///
/// A task waiting on a waker is in no queue, the runtime drops its future
/// through the list when it is dropped. The list holds no references.
pub(crate) struct TaskList {
    head: Cell<*mut Header>,
}

impl TaskList {
    pub(crate) const fn new() -> Self {
        Self {
            head: Cell::new(ptr::null_mut()),
        }
    }

    /// Inserts an unlisted task at the head.
    fn push(&self, h: NonNull<Header>) {
        let hdr = header(h);
        let head = self.head.replace(h.as_ptr());
        hdr.list_prev.set(ptr::null_mut());
        hdr.list_next.set(head);
        if !head.is_null() {
            unsafe { (*head).list_prev.set(h.as_ptr()) };
        }
    }

    /// Unlinks a listed task, the caller clears `LISTED`.
    fn remove(&self, h: NonNull<Header>) {
        let hdr = header(h);
        let (prev, next) = (hdr.list_prev.get(), hdr.list_next.get());
        if prev.is_null() {
            self.head.set(next);
        } else {
            unsafe { (*prev).list_next.set(next) };
        }
        if !next.is_null() {
            unsafe { (*next).list_prev.set(prev) };
        }
    }

    /// Drops the future of the first task, returns `false` if the list is
    /// empty.
    pub(crate) fn close_first(&self) -> bool {
        let Some(h) = NonNull::new(self.head.get()) else {
            return false;
        };
        let hdr = header(h);
        let state = hdr.state.get();
        if state & RUNNING != 0 {
            // polled or dropped up the stack, it is left to its runner
            hdr.state.set(state & !LISTED);
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskList").finish_non_exhaustive()
    }
}

/// A push to the remote queue is in progress.
#[derive(Debug)]
pub(crate) struct Busy;

/// Intrusive MPSC queue of tasks woken on other threads.
///
/// Any thread pushes, the owner thread pops. Dmitry Vyukov's queue: a
/// producer swaps the tail and then links the previous tail to the task, the
/// consumer sees a swapped but unlinked tail as `Busy`. The stub keeps
/// the queue non-empty, so the consumer never pops the last link that a
/// producer may still write to.
#[repr(C)]
pub(crate) struct RemoteQueue {
    /// Producer end, the last pushed link.
    tail: AtomicPtr<Link>,
    // Keep head updates off the producer's cache line.
    _consumer: [super::CacheLine; 0],
    /// Consumer end.
    head: Cell<*mut Link>,
    /// Link of no task, owned by the queue.
    stub: NonNull<Link>,
}

impl RemoteQueue {
    pub(crate) fn new() -> Self {
        let stub = NonNull::from(Box::leak(Box::new(Link {
            next: AtomicPtr::new(ptr::null_mut()),
        })));
        Self {
            tail: AtomicPtr::new(stub.as_ptr()),
            _consumer: [],
            head: Cell::new(stub.as_ptr()),
            stub,
        }
    }

    /// Pushes a task on any thread, the queue takes its reference.
    pub(crate) fn push(&self, task: Remote) {
        // the header starts with the link
        unsafe { self.push_link(ManuallyDrop::new(task).0.cast::<Link>().as_ptr()) };
    }

    /// Appends `link`. Until the previous tail is linked the consumer sees
    /// the queue as `Busy`.
    unsafe fn push_link(&self, link: *mut Link) {
        unsafe { (*link).next.store(ptr::null_mut(), Ordering::Relaxed) };
        let prev = self.tail.swap(link, Ordering::AcqRel);
        unsafe { (*prev).next.store(link, Ordering::Release) };
    }

    /// Pops a task on the owner thread, its reference passes to the caller.
    pub(crate) fn pop(&self) -> Result<Option<Remote>, Busy> {
        let stub = self.stub.as_ptr();
        let mut head = self.head.get();
        let mut next = unsafe { (*head).next.load(Ordering::Acquire) };
        if head == stub {
            // the stub is no task, skip it
            if next.is_null() {
                return if self.tail.load(Ordering::Acquire) == stub {
                    Ok(None)
                } else {
                    Err(Busy)
                };
            }
            self.head.set(next);
            head = next;
            next = unsafe { (*head).next.load(Ordering::Acquire) };
        }
        if next.is_null() {
            // `head` is the last link, re-queue the stub behind it
            if self.tail.load(Ordering::Acquire) != head {
                return Err(Busy);
            }
            unsafe { self.push_link(stub) };
            next = unsafe { (*head).next.load(Ordering::Acquire) };
            if next.is_null() {
                return Err(Busy);
            }
        }
        self.head.set(next);
        Ok(Some(Remote(unsafe {
            NonNull::new_unchecked(head.cast::<Header>())
        })))
    }

    pub(crate) fn is_empty(&self) -> bool {
        let stub = self.stub.as_ptr();
        self.head.get() == stub && self.tail.load(Ordering::Acquire) == stub
    }
}

impl Drop for RemoteQueue {
    fn drop(&mut self) {
        // queued tasks keep the runtime queue alive, the queue is empty
        drop(unsafe { Box::from_raw(self.stub.as_ptr()) });
    }
}

impl fmt::Debug for RemoteQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteQueue").finish_non_exhaustive()
    }
}

/// Join side of a task, owns a reference.
pub(crate) struct Task<T> {
    h: NonNull<Header>,
    _t: PhantomData<T>,
}

unsafe impl<T: Send> Send for Task<T> {}
unsafe impl<T: Send> Sync for Task<T> {}
impl<T> Unpin for Task<T> {}

impl<T> Task<T> {
    /// Returns `true` once the future is dropped, with or without an output.
    pub(crate) fn is_finished(&self) -> bool {
        if is_owner(self.h) {
            header(self.h).state.get() & FINISHED != 0
        } else {
            header(self.h).remote.load(Ordering::Acquire) & R_FINISHED != 0
        }
    }

    /// Cancels the task and drops the join handle, the owner thread drops
    /// the future. The output of a finished task is dropped.
    pub(crate) fn cancel(self) {
        let h = ManuallyDrop::new(self).h;
        if is_owner(h) {
            detach_local(h, true);
        } else {
            detach_remote(h, R_CANCEL);
        }
    }

    /// Drops the join handle, the task keeps running.
    pub(crate) fn detach(self) {
        drop(self);
    }

    /// Polls the task output, `None` if the task was cancelled or panicked.
    pub(crate) fn poll_join(&mut self, cx: &Context<'_>) -> Poll<Option<T>> {
        let hdr = header(self.h);
        if is_owner(self.h) {
            if hdr.state.get() & FINISHED == 0 {
                // the waker callbacks may poll the runtime and complete the task
                drop(unsafe { hdr.awaiter.register(cx.waker()) });
                if hdr.state.get() & FINISHED == 0 {
                    return Poll::Pending;
                }
                // completed by a waker callback, nothing waits any more
                drop(unsafe { hdr.awaiter.take() });
            }
        } else {
            // `R_REGISTERING` keeps the owner thread off the awaiter while it
            // is written
            let mut cur = hdr.remote.load(Ordering::Acquire);
            while cur & R_FINISHED == 0 {
                if let Err(v) = hdr.remote.compare_exchange_weak(
                    cur,
                    cur | R_REGISTERING,
                    Ordering::Acquire,
                    Ordering::Acquire,
                ) {
                    cur = v;
                    continue;
                }
                let guard = Registering(&hdr.remote);
                let old = unsafe { hdr.awaiter.register(cx.waker()) };
                mem::forget(guard);
                let prev = hdr.remote.fetch_and(!R_REGISTERING, Ordering::AcqRel);
                // dropped outside of the registration, it may wait for the owner thread
                drop(old);
                if prev & R_FINISHED == 0 {
                    return Poll::Pending;
                }
                // completed while registering, the owner thread skipped the awaiter
                drop(unsafe { hdr.awaiter.take() });
                break;
            }
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
    fn drop(&mut self) {
        if is_owner(self.h) {
            detach_local(self.h, false);
        } else {
            // the owner thread drops the awaiter on completion
            detach_remote(self.h, 0);
        }
    }
}

/// Drops a join handle on the owner thread, `cancel` also drops the future.
fn detach_local(h: NonNull<Header>, cancel: bool) {
    let hdr = header(h);
    let state = hdr.state.get();
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
        hdr.state.set(state | SCHEDULED);
        // the handle reference passes to the queue
        hdr.queue.schedule_local(Runnable(h));
    } else {
        hdr.state.set(state);
        release_local(h);
    }
    drop(awaiter);
}

/// Drops a join handle on another thread, sets `flags` like
/// [`remote_signal`].
///
/// The output of a finished task is dropped on this thread, a `Task<T>` on
/// another thread implies `T: Send`. Otherwise the owner thread drops it on
/// completion.
fn detach_remote(h: NonNull<Header>, flags: u32) {
    let remote = &header(h).remote;
    let mut cur = remote.load(Ordering::Acquire);
    loop {
        if cur & R_FINISHED != 0 {
            drop_output(h);
            return;
        }
        if delta(cur).abs() > MAX_REFS {
            abort();
        }
        // `R_DETACHED` tells the owner thread to drop the output. The fast
        // path of `remote_signal`, this is not the last reference
        let release = flags == 0 && (cur & R_QUEUED != 0 || delta(cur) > 0);
        let new = if release {
            (cur | R_DETACHED).wrapping_sub(R_REF)
        } else {
            cur | R_DETACHED
        };
        match remote.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) if release => return,
            Ok(_) => break,
            Err(v) => cur = v,
        }
    }
    remote_signal(h, flags, true);
}

impl<T> fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("location", &header(self.h).location)
            .finish()
    }
}

#[cfg(all(test, ntex_loom))]
#[test]
#[should_panic(expected = "currently writing to cell")]
fn task_loom_shared_cell_access() {
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
