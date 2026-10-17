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
//! word when it pops the task. The task is freed on the owner thread only,
//! once the local count and the delta are both zero.
//!
//! The runtime queues are intrusive, a task links itself into the local
//! queue and into the remote queue.
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

    fn get(&self) -> *mut T {
        #[cfg(not(ntex_loom))]
        return self.0.get();
        #[cfg(ntex_loom)]
        return self.0.with_mut(|p| p);
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
const R_SHIFT: u32 = 6;
const R_REF: u32 = 1 << R_SHIFT;
const R_FLAGS: u32 = R_REF - 1;
/// Half of the delta range, racing increments stay in range until one of
/// them aborts.
const MAX_REFS: i32 = i32::MAX >> (R_SHIFT + 1);

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
    vtable: &'static VTable,
    queue: Arc<RunnableQueue>,
    location: SpawnLocation,
    awaiter: Awaiter,
    remote: AtomicU32,
    /// Flags and the self-wake count.
    state: Cell<u32>,
    refs: Cell<u32>,
    /// Start of the self-wake streak, milliseconds since the runtime epoch.
    streak_start: Cell<u32>,
}

#[cfg(all(target_pointer_width = "64", not(ntex_loom)))]
const _: () = assert!(mem::size_of::<Header>() == 64);

enum Stage<F: Future> {
    Running(F),
    Finished(F::Output),
    Consumed,
}

#[repr(C)]
struct RawTask<F: Future> {
    header: Header,
    stage: SharedCell<Stage<F>>,
}

struct Fns<F>(PhantomData<F>);

impl<F: Future> Fns<F> {
    const VTABLE: VTable = VTable {
        poll: Self::poll,
        drop_future: Self::drop_future,
        take_output: Self::take_output,
        drop_output: Self::drop_output,
        dealloc: Self::dealloc,
    };

    unsafe fn stage(h: NonNull<Header>) -> *mut Stage<F> {
        unsafe { (*h.cast::<RawTask<F>>().as_ptr()).stage.get() }
    }

    unsafe fn poll(h: NonNull<Header>, cx: &mut Context<'_>) -> bool {
        unsafe {
            let stage = Self::stage(h);
            let Stage::Running(fut) = &mut *stage else {
                unreachable!()
            };
            match Pin::new_unchecked(fut).poll(cx) {
                Poll::Ready(out) => {
                    Self::drop_future(h);
                    ptr::write(stage, Stage::Finished(out));
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
            let guard = Guard(Self::stage(h));
            ptr::drop_in_place(guard.0);
        }
    }

    unsafe fn take_output(h: NonNull<Header>, out: *mut ()) {
        unsafe {
            let stage = Self::stage(h);
            if matches!(*stage, Stage::Finished(_))
                && let Stage::Finished(v) = ptr::replace(stage, Stage::Consumed)
            {
                *out.cast::<Option<F::Output>>() = Some(v);
            }
        }
    }

    unsafe fn drop_output(h: NonNull<Header>) {
        unsafe {
            let stage = Self::stage(h);
            if matches!(*stage, Stage::Finished(_)) {
                drop(ptr::replace(stage, Stage::Consumed));
            }
        }
    }

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

    /// # Safety
    ///
    /// The caller should have exclusive access to the awaiter.
    unsafe fn take(&self) -> Option<Waker> {
        let p = unsafe { self.0.get().replace(ptr::null_mut()) };
        if p.is_null() {
            None
        } else if p.addr() & BOXED != 0 {
            let w = unsafe { Box::from_raw(p.map_addr(|a| a & !BOXED).cast::<Waker>()) };
            Some(*w)
        } else {
            Some(unsafe { Waker::from_raw(RawWaker::new(p, &WAKER_VTABLE)) })
        }
    }

    /// # Safety
    ///
    /// The caller should have exclusive access to the awaiter.
    unsafe fn register(&self, waker: &Waker) {
        let cur = unsafe { *self.0.get() };
        let new = if ptr::eq(waker.vtable(), &raw const WAKER_VTABLE) {
            if ptr::eq(cur, waker.data()) {
                return;
            }
            // the stored pointer owns the cloned reference
            ManuallyDrop::new(waker.clone()).data().cast_mut()
        } else {
            if cur.addr() & BOXED != 0
                && unsafe { &*cur.map_addr(|a| a & !BOXED).cast::<Waker>() }.will_wake(waker)
            {
                return;
            }
            Box::into_raw(Box::new(waker.clone()))
                .cast::<()>()
                .map_addr(|a| a | BOXED)
        };
        drop(unsafe { self.take() });
        unsafe { *self.0.get() = new };
    }
}

impl Drop for Awaiter {
    fn drop(&mut self) {
        drop(unsafe { self.take() });
    }
}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(clone_waker, wake, wake_by_ref, drop_waker);

#[inline]
fn header<'a>(h: NonNull<Header>) -> &'a Header {
    unsafe { h.as_ref() }
}

#[inline]
fn is_owner(h: NonNull<Header>) -> bool {
    header(h).queue.id == current_thread_id()
}

unsafe fn clone_waker(ptr: *const ()) -> RawWaker {
    let h = unsafe { NonNull::new_unchecked(ptr.cast_mut().cast::<Header>()) };
    if is_owner(h) {
        inc_local(h);
    } else {
        let prev = header(h).remote.fetch_add(R_REF, Ordering::Relaxed);
        if delta(prev) > MAX_REFS {
            abort();
        }
    }
    RawWaker::new(ptr, &WAKER_VTABLE)
}

unsafe fn wake(ptr: *const ()) {
    let h = unsafe { NonNull::new_unchecked(ptr.cast_mut().cast::<Header>()) };
    if is_owner(h) {
        wake_local(h, true);
    } else {
        remote_signal(h, R_WOKEN, true);
    }
}

unsafe fn wake_by_ref(ptr: *const ()) {
    let h = unsafe { NonNull::new_unchecked(ptr.cast_mut().cast::<Header>()) };
    if is_owner(h) {
        wake_local(h, false);
    } else {
        remote_signal(h, R_WOKEN, false);
    }
}

unsafe fn drop_waker(ptr: *const ()) {
    release(unsafe { NonNull::new_unchecked(ptr.cast_mut().cast::<Header>()) });
}

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
    let mut cur = remote.load(Ordering::Relaxed);
    loop {
        if delta(cur).abs() > MAX_REFS {
            abort();
        }
        let set = if cur & R_FINISHED == 0 { flags & !cur } else { 0 };
        let (new, push) = if cur & R_QUEUED != 0 {
            if owned {
                ((cur | set).wrapping_sub(R_REF), false)
            } else if set != 0 {
                (cur | set, false)
            } else {
                return;
            }
        } else if owned && set == 0 && delta(cur) > 0 {
            // a live task has local references outside of `last_ref`, which
            // re-reads the delta, so this is not the last reference
            (cur.wrapping_sub(R_REF), false)
        } else if owned {
            // the reference passes to the queue
            (cur | set | R_QUEUED, true)
        } else if set != 0 {
            ((cur | set | R_QUEUED).wrapping_add(R_REF), true)
        } else {
            return;
        };
        match remote.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) if push => break,
            Ok(_) => return,
            Err(v) => cur = v,
        }
    }
    // the owner thread may free the task once it is queued, the queue must
    // outlive the call
    let queue = header(h).queue.clone();
    queue.schedule_remote(Remote(h));
}

/// Releases a reference on the owner thread.
fn release_local(h: NonNull<Header>) {
    let hdr = header(h);
    let refs = hdr.refs.get() - 1;
    hdr.refs.set(refs);
    if refs == 0 {
        last_ref(h);
    }
}

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

    let state = hdr.state.get();
    if state & FINISHED == 0 {
        // nothing can wake the task, the runner drops the future
        hdr.refs.set(1);
        hdr.state.set(state | SCHEDULED | CLOSE);
        hdr.queue.schedule_local(Runnable(h));
    } else {
        unsafe { (hdr.vtable.dealloc)(h) };
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
            hdr.state.set(hdr.state.get() | FINISHED);
            // the future is dropped, there is no output
            complete(self.0);
            release_local(self.0);
        }
    }

    let finish = Finish(h);
    unsafe { (header(finish.0).vtable.drop_future)(finish.0) };
}

/// Publishes completion and wakes the awaiter, on the owner thread.
///
/// Returns `true` if the join handle is dropped, the output has no owner.
fn complete(h: NonNull<Header>) -> bool {
    let hdr = header(h);
    let prev = hdr.remote.fetch_or(R_FINISHED, Ordering::AcqRel);
    // a registering join handle sees `R_FINISHED` and handles the awaiter
    if prev & R_REGISTERING == 0
        && let Some(w) = unsafe { hdr.awaiter.take() }
    {
        w.wake();
    }
    hdr.state.get() & DETACHED != 0 || prev & R_DETACHED != 0
}

/// Releases a reference on drop, also while unwinding from an output drop.
struct Release(NonNull<Header>);

impl Drop for Release {
    fn drop(&mut self) {
        release(self.0);
    }
}

/// Drops the output, then releases a reference of the caller.
fn drop_output(h: NonNull<Header>) {
    let release = Release(h);
    unsafe { (header(release.0).vtable.drop_output)(release.0) };
}

/// Spawns a task and schedules it on its runtime.
///
/// # Safety
///
/// The caller should ensure the captured lifetime is long enough.
pub(crate) unsafe fn spawn<F: Future>(
    future: F,
    queue: Arc<RunnableQueue>,
    location: SpawnLocation,
) -> Task<F::Output> {
    let h = unsafe { create(future, queue, location, false) };
    Task { h, _t: PhantomData }
}

/// Spawns a task without a join handle, the output is dropped on completion.
///
/// # Safety
///
/// The caller should ensure the captured lifetime is long enough.
pub(crate) unsafe fn spawn_detached<F: Future>(
    future: F,
    queue: Arc<RunnableQueue>,
    location: SpawnLocation,
) {
    unsafe { create(future, queue, location, true) };
}

unsafe fn create<F: Future>(
    future: F,
    queue: Arc<RunnableQueue>,
    location: SpawnLocation,
    detached: bool,
) -> NonNull<Header> {
    let local = queue.id == current_thread_id();
    let raw = Box::new(RawTask {
        header: Header {
            link: Link {
                next: AtomicPtr::new(ptr::null_mut()),
            },
            next: Cell::new(ptr::null_mut()),
            vtable: &Fns::<F>::VTABLE,
            queue,
            location,
            awaiter: Awaiter::new(),
            remote: AtomicU32::new(if local { 0 } else { R_QUEUED | R_WOKEN }),
            state: Cell::new(
                if local { SCHEDULED } else { 0 } | if detached { DETACHED } else { 0 },
            ),
            // the queue and the join handle
            refs: Cell::new(if detached { 1 } else { 2 }),
            streak_start: Cell::new(0),
        },
        stage: SharedCell::new(Stage::Running(future)),
    });
    let h = NonNull::from(Box::leak(raw)).cast::<Header>();
    let queue = &header(h).queue;
    if local {
        queue.schedule_local(Runnable(h));
    } else if detached {
        // the owner thread may free the task once it is queued
        queue.clone().schedule_remote(Remote(h));
    } else {
        // the join handle keeps the queue alive
        queue.schedule_remote(Remote(h));
    }
    h
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

        let h = self.0;
        mem::forget(self);
        let hdr = header(h);

        let state = hdr.state.get() & !SCHEDULED;
        if state & (CLOSE | FINISHED) != 0 {
            hdr.state.set(state);
            if state & FINISHED == 0 {
                close(h);
            } else {
                release_local(h);
            }
            return false;
        }
        hdr.state.set(state | RUNNING);

        let waker = ManuallyDrop::new(unsafe {
            Waker::from_raw(RawWaker::new(h.as_ptr().cast(), &WAKER_VTABLE))
        });
        let mut cx = Context::from_waker(&waker);
        let guard = Guard(h);
        let ready = unsafe { (hdr.vtable.poll)(h, &mut cx) };
        mem::forget(guard);

        let state = hdr.state.get() & !RUNNING;
        if ready {
            hdr.state.set((state | FINISHED) & !(SCHEDULED | CLOSE));
            if complete(h) {
                drop_output(h);
                return false;
            }
        } else if state & CLOSE != 0 {
            hdr.state.set(state & !SCHEDULED);
            close(h);
            return false;
        } else if state & SCHEDULED != 0 {
            hdr.state.set(state);
            // the run reference passes to the queue
            hdr.queue.schedule_local(Runnable(h));
            return true;
        } else {
            hdr.state.set(state);
        }
        release_local(h);
        false
    }
}

impl Drop for Runnable {
    fn drop(&mut self) {
        // a queue dropped on another thread leaks its tasks
        if is_owner(self.0) {
            let hdr = header(self.0);
            let state = hdr.state.get();
            hdr.state.set(state & !SCHEDULED);
            if state & FINISHED == 0 {
                close(self.0);
            } else {
                release_local(self.0);
            }
        }
    }
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
        let h = self.0;
        mem::forget(self);
        let hdr = header(h);

        let cur = hdr
            .remote
            .fetch_and(R_FINISHED | R_REGISTERING | R_DETACHED, Ordering::AcqRel);
        let refs = i64::from(hdr.refs.get()) + i64::from(delta(cur));
        hdr.refs
            .set(u32::try_from(refs).unwrap_or_else(|_| abort()));

        let mut state = hdr.state.get();
        if cur & R_CANCEL != 0 && state & FINISHED == 0 {
            state |= CLOSE;
            hdr.state.set(state);
        }
        if cur & (R_WOKEN | R_CANCEL) != 0 && state & (SCHEDULED | RUNNING | FINISHED) == 0 {
            // the queue reference passes to the runnable
            hdr.state.set(state | SCHEDULED);
            Some(Runnable(h))
        } else {
            release_local(h);
            None
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        // a queue dropped on another thread leaks its tasks
        if is_owner(self.0) {
            drop(Remote(self.0).into_runnable());
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
pub(crate) struct RemoteQueue {
    tail: AtomicPtr<Link>,
    /// Consumer end.
    head: Cell<*mut Link>,
    stub: NonNull<Link>,
}

impl RemoteQueue {
    pub(crate) fn new() -> Self {
        let stub = NonNull::from(Box::leak(Box::new(Link {
            next: AtomicPtr::new(ptr::null_mut()),
        })));
        Self {
            tail: AtomicPtr::new(stub.as_ptr()),
            head: Cell::new(stub.as_ptr()),
            stub,
        }
    }

    pub(crate) fn push(&self, task: Remote) {
        // the header starts with the link
        unsafe { self.push_link(ManuallyDrop::new(task).0.cast::<Link>().as_ptr()) };
    }

    unsafe fn push_link(&self, link: *mut Link) {
        unsafe { (*link).next.store(ptr::null_mut(), Ordering::Relaxed) };
        let prev = self.tail.swap(link, Ordering::AcqRel);
        unsafe { (*prev).next.store(link, Ordering::Release) };
    }

    /// Pops a task on the owner thread.
    pub(crate) fn pop(&self) -> Result<Option<Remote>, Busy> {
        let stub = self.stub.as_ptr();
        let mut head = self.head.get();
        let mut next = unsafe { (*head).next.load(Ordering::Acquire) };
        if head == stub {
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
    pub(crate) fn is_finished(&self) -> bool {
        if is_owner(self.h) {
            header(self.h).state.get() & FINISHED != 0
        } else {
            header(self.h).remote.load(Ordering::Acquire) & R_FINISHED != 0
        }
    }

    pub(crate) fn cancel(self) {
        let h = self.h;
        mem::forget(self);
        if is_owner(h) {
            let hdr = header(h);
            let state = hdr.state.get();
            if state & FINISHED != 0 {
                drop_output(h);
                return;
            }
            // a running task may still complete
            hdr.state.set(state | CLOSE | DETACHED);
            if state & (SCHEDULED | RUNNING) == 0 {
                hdr.state.set(state | CLOSE | DETACHED | SCHEDULED);
                // the handle reference passes to the queue
                hdr.queue.schedule_local(Runnable(h));
                return;
            }
            release_local(h);
        } else {
            detach_remote(h, R_CANCEL);
        }
    }

    pub(crate) fn detach(self) {
        drop(self);
    }

    /// Polls the task output, `None` if the task was cancelled or panicked.
    pub(crate) fn poll_join(&mut self, cx: &Context<'_>) -> Poll<Option<T>> {
        let hdr = header(self.h);
        if is_owner(self.h) {
            if hdr.state.get() & FINISHED == 0 {
                unsafe { hdr.awaiter.register(cx.waker()) };
                return Poll::Pending;
            }
        } else if hdr.remote.load(Ordering::Acquire) & R_FINISHED == 0 {
            let mut cur = hdr.remote.load(Ordering::Acquire);
            loop {
                if cur & R_FINISHED != 0 {
                    return Poll::Ready(self.take_output());
                }
                match hdr.remote.compare_exchange_weak(
                    cur,
                    cur | R_REGISTERING,
                    Ordering::Acquire,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(v) => cur = v,
                }
            }
            unsafe { hdr.awaiter.register(cx.waker()) };
            let prev = hdr.remote.fetch_and(!R_REGISTERING, Ordering::AcqRel);
            if prev & R_FINISHED == 0 {
                return Poll::Pending;
            }
            // completed while registering, the owner thread skipped the awaiter
            drop(unsafe { hdr.awaiter.take() });
        }
        Poll::Ready(self.take_output())
    }

    fn take_output(&mut self) -> Option<T> {
        let mut out = None;
        unsafe { (header(self.h).vtable.take_output)(self.h, (&raw mut out).cast()) };
        out
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        let h = self.h;
        if is_owner(h) {
            let hdr = header(h);
            let state = hdr.state.get();
            if state & FINISHED != 0 {
                drop_output(h);
                return;
            }
            hdr.state.set(state | DETACHED);
            release_local(h);
        } else {
            detach_remote(h, 0);
        }
    }
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
        // the fast path of `remote_signal`, this is not the last reference
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
