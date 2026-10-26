//! Tasks submitted from other threads through [`crate::Handle`].
//!
//! A submission allocates one [`Job`], holding the future and later its
//! output. The owner thread polls the future in place through a [`Runner`]
//! task, the joining [`RemoteJoin`] takes the output from the same allocation.
use std::cell::UnsafeCell;
use std::sync::{Arc, atomic::AtomicU8, atomic::Ordering};
use std::task::{Context, Poll, ready};
use std::{future::Future, mem, pin::Pin};

use atomic_waker::AtomicWaker;

/// The job has finished, its stage is no longer accessed by the owner.
const DONE: u8 = 1;
/// The joiner requested cancellation.
const CANCELLED: u8 = 2;
/// A joiner may cancel the job, the runner registers its waker.
const JOINABLE: u8 = 4;

enum Stage<F: Future> {
    Running(F),
    Done(F::Output),
    Empty,
}

/// A future submitted to an owner thread, and its output.
pub(crate) struct Job<F: Future> {
    state: AtomicU8,
    /// Woken once the job is done.
    join_waker: AtomicWaker,
    /// Woken to let the runner observe a cancellation.
    run_waker: AtomicWaker,
    stage: UnsafeCell<Stage<F>>,
}

// The owner accesses `stage` until it publishes `DONE`, the joiner only after
// observing it. The future is polled and dropped on the owner thread, unless
// the submission is rejected before it is published.
unsafe impl<F: Future + Send> Send for Job<F> where F::Output: Send {}
unsafe impl<F: Future + Send> Sync for Job<F> where F::Output: Send {}

impl<F: Future> Job<F> {
    /// Allocates a job, `joinable` jobs can be cancelled by their joiner.
    pub(crate) fn new(future: F, joinable: bool) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(if joinable { JOINABLE } else { 0 }),
            join_waker: AtomicWaker::new(),
            run_waker: AtomicWaker::new(),
            stage: UnsafeCell::new(Stage::Running(future)),
        })
    }

    /// Drops the future without an output and publishes completion.
    ///
    /// Called on the owner thread, or on the submitting thread if the job was
    /// never published.
    pub(crate) fn close(&self) {
        if self.state.load(Ordering::Acquire) & DONE == 0 {
            self.finish(Stage::Empty);
        }
    }

    /// Drops the pinned future in place, then publishes the new stage even on unwind.
    fn finish(&self, stage: Stage<F>) {
        struct Complete<'a, F: Future>(&'a Job<F>);

        impl<F: Future> Drop for Complete<'_, F> {
            fn drop(&mut self) {
                self.0.complete();
            }
        }

        let _complete = Complete(self);
        // SAFETY: before DONE, only the owner (or unpublished submitter) accesses
        // the stage. Assignment drops the future in place and installs the new
        // stage even if its destructor panics, before the guard publishes DONE.
        unsafe { *self.stage.get() = stage };
    }

    fn complete(&self) {
        self.state.fetch_or(DONE, Ordering::AcqRel);
        // the runner's waker must not retain its task
        drop(self.run_waker.take());
        self.join_waker.wake();
    }
}

/// Owner-thread task polling a [`Job`] in place.
pub(crate) struct Runner<F: Future>(pub(crate) Arc<Job<F>>);

impl<F: Future> Future for Runner<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let job = &*self.0;
        let state = job.state.load(Ordering::Acquire);
        if state & DONE != 0 {
            return Poll::Ready(());
        }
        if state & CANCELLED != 0 {
            job.close();
            return Poll::Ready(());
        }
        if state & JOINABLE != 0 {
            job.run_waker.register(cx.waker());
            if job.state.load(Ordering::Acquire) & CANCELLED != 0 {
                job.close();
                return Poll::Ready(());
            }
        }
        // SAFETY: not done, only the owner accesses the stage
        let slot = unsafe { &mut *job.stage.get() };
        let Stage::Running(fut) = slot else {
            unreachable!("job polled after completion")
        };
        // SAFETY: the future stays in this allocation until it is dropped in place.
        let out = ready!(unsafe { Pin::new_unchecked(fut) }.poll(cx));
        job.finish(Stage::Done(out));
        Poll::Ready(())
    }
}

impl<F: Future> Drop for Runner<F> {
    /// Drops an unfinished future on the owner thread, the joiner gets an error.
    fn drop(&mut self) {
        self.0.close();
    }
}

trait JoinJob<T>: Send + Sync {
    fn poll_join(&self, cx: &mut Context<'_>) -> Poll<Option<T>>;
    fn cancel(&self);
    fn detach(&self);
    fn is_finished(&self) -> bool;
}

impl<F> JoinJob<F::Output> for Job<F>
where
    F: Future + Send,
    F::Output: Send,
{
    fn poll_join(&self, cx: &mut Context<'_>) -> Poll<Option<F::Output>> {
        if self.state.load(Ordering::Acquire) & DONE == 0 {
            self.join_waker.register(cx.waker());
            if self.state.load(Ordering::Acquire) & DONE == 0 {
                return Poll::Pending;
            }
        }
        // SAFETY: done, the owner no longer accesses the stage and the
        // joiner is unique
        match mem::replace(unsafe { &mut *self.stage.get() }, Stage::Empty) {
            Stage::Done(out) => Poll::Ready(Some(out)),
            _ => Poll::Ready(None),
        }
    }

    fn cancel(&self) {
        self.state.fetch_or(CANCELLED, Ordering::AcqRel);
        self.run_waker.wake();
    }

    fn detach(&self) {
        // no joiner can cancel, the runner stops registering its waker
        self.state.fetch_and(!JOINABLE, Ordering::AcqRel);
    }

    fn is_finished(&self) -> bool {
        self.state.load(Ordering::Acquire) & DONE != 0
    }
}

/// The joining side of a [`Job`].
pub(crate) struct RemoteJoin<T>(Arc<dyn JoinJob<T>>);

impl<T> RemoteJoin<T> {
    pub(crate) fn new<F>(job: Arc<Job<F>>) -> Self
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        Self(job)
    }

    /// Resolves to `None` if the job ended without an output.
    pub(crate) fn poll_join(&self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.0.poll_join(cx)
    }

    /// Requests cancellation, the owner drops the future.
    pub(crate) fn cancel(&self) {
        self.0.cancel();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.0.is_finished()
    }
}

impl<T> Drop for RemoteJoin<T> {
    fn drop(&mut self) {
        self.0.detach();
    }
}

impl<T> std::fmt::Debug for RemoteJoin<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteJoin")
            .field("finished", &self.is_finished())
            .finish()
    }
}

#[cfg(any(feature = "tokio", feature = "compio"))]
pub(crate) use self::spawner::Spawner;

#[cfg(any(feature = "tokio", feature = "compio"))]
mod spawner {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::{fmt, future::Future, pin::Pin, sync::Arc, thread};

    use atomic_waker::AtomicWaker;
    use crossbeam_queue::SegQueue;

    use super::{Job, Runner};
    use crate::rt::{ThreadId, current_thread_id, try_current_thread_id};

    /// Jobs started per poll of the drain task.
    const BATCH: usize = 128;
    /// The remaining bits count admitted publications.
    const CLOSED: usize = 1 << (usize::BITS - 1);

    trait RunJob: Send + Sync {
        fn spawn_local(self: Arc<Self>);
        fn close(&self);
    }

    impl<F> RunJob for Job<F>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        fn spawn_local(self: Arc<Self>) {
            crate::spawn_detached(Runner(self));
        }

        fn close(&self) {
            Job::close(self);
        }
    }

    /// FIFO of jobs submitted to an arbiter thread from other threads.
    pub(crate) struct Spawner {
        owner: ThreadId,
        /// Closed bit and count of publications in progress.
        admission: AtomicUsize,
        /// Coalesces drain task wake ups.
        notified: AtomicBool,
        waker: AtomicWaker,
        queue: SegQueue<Arc<dyn RunJob>>,
    }

    impl Spawner {
        /// Creates a spawner owned by the current thread and starts its drain
        /// task on the current runtime.
        pub(crate) fn start() -> Arc<Self> {
            let spawner = Arc::new(Self {
                owner: current_thread_id(),
                admission: AtomicUsize::new(0),
                notified: AtomicBool::new(false),
                waker: AtomicWaker::new(),
                queue: SegQueue::new(),
            });
            crate::spawn_detached(Drain(spawner.clone()));
            spawner
        }

        /// Returns whether the current thread owns the spawner.
        pub(crate) fn is_owner(&self) -> bool {
            Some(self.owner) == try_current_thread_id()
        }

        /// Publishes a job, a rejected job is closed on the current thread.
        pub(crate) fn submit<F>(&self, job: Arc<Job<F>>)
        where
            F: Future + Send + 'static,
            F::Output: Send + 'static,
        {
            if !self.admit() {
                job.close();
                return;
            }
            self.queue.push(job);
            self.admission.fetch_sub(1, Ordering::Release);
            if !self.notified.swap(true, Ordering::AcqRel) {
                self.waker.wake();
            }
        }

        fn admit(&self) -> bool {
            let mut state = self.admission.load(Ordering::Acquire);
            loop {
                if state & CLOSED != 0 {
                    return false;
                }
                match self.admission.compare_exchange_weak(
                    state,
                    state + 1,
                    Ordering::Acquire,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return true,
                    Err(v) => state = v,
                }
            }
        }

        /// Rejects new jobs and closes queued ones on the owner thread.
        fn close(&self) {
            self.admission.fetch_or(CLOSED, Ordering::AcqRel);
            while self.admission.load(Ordering::Acquire) != CLOSED {
                thread::yield_now();
            }
            while let Some(job) = self.queue.pop() {
                job.close();
            }
        }
    }

    impl fmt::Debug for Spawner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Spawner")
                .field("owner", &self.owner)
                .finish_non_exhaustive()
        }
    }

    /// Owner-thread task starting submitted jobs as local tasks.
    struct Drain(Arc<Spawner>);

    impl Future for Drain {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            let spawner = &*self.0;
            spawner.waker.register(cx.waker());
            // a job pushed after this swap notifies again
            spawner.notified.swap(false, Ordering::AcqRel);
            for _ in 0..BATCH {
                match spawner.queue.pop() {
                    Some(job) => job.spawn_local(),
                    None => return Poll::Pending,
                }
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    impl Drop for Drain {
        fn drop(&mut self) {
            self.0.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::marker::PhantomPinned;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::task::{Wake, Waker};

    use super::*;

    #[derive(Default)]
    struct Probe {
        drops: AtomicUsize,
        output_drops: AtomicUsize,
        moved: AtomicBool,
        wakes: AtomicUsize,
        early_wake: AtomicBool,
    }

    impl Wake for Probe {
        fn wake(self: Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            self.early_wake
                .fetch_or(self.drops.load(Ordering::SeqCst) == 0, Ordering::SeqCst);
        }
    }

    struct Output(Arc<Probe>);

    impl Drop for Output {
        fn drop(&mut self) {
            self.0.output_drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct PinnedFuture {
        address: Cell<usize>,
        probe: Arc<Probe>,
        ready: bool,
        panics: bool,
        _pin: PhantomPinned,
    }

    impl Future for PinnedFuture {
        type Output = Output;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Output> {
            let this = self.as_ref().get_ref();
            this.address.set(std::ptr::from_ref(this).addr());
            if this.ready {
                Poll::Ready(Output(this.probe.clone()))
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for PinnedFuture {
        fn drop(&mut self) {
            self.probe.moved.store(
                self.address.get() != std::ptr::from_ref(self).addr(),
                Ordering::SeqCst,
            );
            self.probe.drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panics, "future drop");
        }
    }

    fn pinned_job(ready: bool, panics: bool) -> (Arc<Job<PinnedFuture>>, Arc<Probe>) {
        let probe = Arc::new(Probe::default());
        let job = Job::new(
            PinnedFuture {
                address: Cell::new(0),
                probe: probe.clone(),
                ready,
                panics,
                _pin: PhantomPinned,
            },
            true,
        );
        (job, probe)
    }

    #[test]
    fn completion_drops_pinned_future_in_place() {
        for panics in [false, true] {
            let (job, probe) = pinned_job(true, panics);
            let mut runner = Runner(job.clone());
            let join = RemoteJoin::new(job.clone());
            let waker = Waker::from(probe.clone());
            let mut cx = Context::from_waker(&waker);
            assert!(join.poll_join(&mut cx).is_pending());

            let result = catch_unwind(AssertUnwindSafe(|| {
                assert!(
                    Pin::new(&mut runner)
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_ready()
                );
            }));
            assert_eq!(result.is_err(), panics);
            assert!(join.is_finished());
            assert!(!probe.moved.load(Ordering::SeqCst));
            assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
            assert_eq!(probe.wakes.load(Ordering::SeqCst), 1);
            assert!(!probe.early_wake.load(Ordering::SeqCst));
            let Poll::Ready(Some(output)) = join.poll_join(&mut cx) else {
                panic!("completed output lost");
            };
            assert!(matches!(join.poll_join(&mut cx), Poll::Ready(None)));
            drop((runner, join, job));
            assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
            assert_eq!(probe.output_drops.load(Ordering::SeqCst), 0);
            drop(output);
            assert_eq!(probe.output_drops.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn cancellation_drops_pinned_future_in_place() {
        for cancel in [false, true] {
            for panics in [false, true] {
                let (job, probe) = pinned_job(false, panics);
                let mut runner = Runner(job.clone());
                let join = RemoteJoin::new(job.clone());
                let waker = Waker::from(probe.clone());
                let mut cx = Context::from_waker(&waker);
                assert!(join.poll_join(&mut cx).is_pending());
                assert!(
                    Pin::new(&mut runner)
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );

                let result = catch_unwind(AssertUnwindSafe(|| {
                    if cancel {
                        join.cancel();
                        assert!(
                            Pin::new(&mut runner)
                                .poll(&mut Context::from_waker(Waker::noop()))
                                .is_ready()
                        );
                    }
                    drop(runner);
                }));
                assert_eq!(result.is_err(), panics);
                assert!(join.is_finished());
                assert!(!probe.moved.load(Ordering::SeqCst));
                assert!(matches!(join.poll_join(&mut cx), Poll::Ready(None)));
                assert_eq!(probe.wakes.load(Ordering::SeqCst), 1);
                assert!(!probe.early_wake.load(Ordering::SeqCst));
                job.close();
                drop((join, job));
                assert_eq!(probe.drops.load(Ordering::SeqCst), 1);
                assert_eq!(probe.output_drops.load(Ordering::SeqCst), 0);
            }
        }
    }
}
