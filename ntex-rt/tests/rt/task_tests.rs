//! Ported async-task scenarios for the runtime task.
//!
//! The test thread is the owner thread, it runs tasks by hand and counts
//! the queued tasks to check each schedule.

// `clone().wake()` exercises the by-value wake path
#![allow(clippy::waker_clone_wake)]
use std::future::poll_fn;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Barrier, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;

use super::{Arc, Future, Location, Notify, Ordering, RunnableQueue, Runtime, io};
use crate::rt::task::{self, Runnable, Task};

#[derive(Debug)]
struct NoopNotify;

impl Notify for NoopNotify {
    fn notify(&self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default, Clone)]
struct Counter(Arc<AtomicUsize>);

impl Counter {
    fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    fn inc(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn token(&self) -> Token {
        Token(self.clone())
    }
}

/// Increments the counter on drop.
#[derive(Debug)]
struct Token(Counter);

impl Drop for Token {
    fn drop(&mut self) {
        self.0.inc();
    }
}

struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        self.0.take().unwrap()();
    }
}

fn pending_with_drop(f: impl FnOnce()) -> impl Future<Output = ()> {
    let hook = OnDrop(Some(f));
    async move {
        let _hook = hook;
        std::future::pending::<()>().await;
    }
}

#[derive(Default)]
struct CountWaker(AtomicUsize);

impl std::task::Wake for CountWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Res {
    Pending,
    Ready,
    Panic,
}

/// Event counters of a test future.
#[derive(Default, Clone)]
struct Probe {
    polls: Counter,
    drops: Counter,
    outputs: Counter,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl Probe {
    fn future(&self, res: Res) -> Fut {
        Fut {
            p: self.clone(),
            res,
            gate: None,
            _drop: self.drops.token(),
        }
    }

    /// The first poll waits twice on the barrier, another thread acts in
    /// between.
    fn gated(&self, res: Res, gate: &Arc<Barrier>) -> Fut {
        Fut {
            gate: Some(gate.clone()),
            ..self.future(res)
        }
    }

    /// Takes the waker stored by the last poll.
    fn waker(&self) -> Waker {
        self.waker.lock().unwrap().take().unwrap()
    }

    fn waker_clone(&self) -> Waker {
        self.waker.lock().unwrap().clone().unwrap()
    }

    #[track_caller]
    fn check(&self, polls: usize, drops: usize, outputs: usize) {
        assert_eq!(
            (self.polls.get(), self.drops.get(), self.outputs.get()),
            (polls, drops, outputs),
            "(polls, drops, outputs)"
        );
    }
}

struct Fut {
    p: Probe,
    res: Res,
    gate: Option<Arc<Barrier>>,
    _drop: Token,
}

impl Future for Fut {
    type Output = Token;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Token> {
        let this = self.get_mut();
        *this.p.waker.lock().unwrap() = Some(cx.waker().clone());
        this.p.polls.inc();
        if let Some(gate) = this.gate.take() {
            gate.wait();
            gate.wait();
        }
        match this.res {
            Res::Pending => Poll::Pending,
            Res::Ready => Poll::Ready(this.p.outputs.token()),
            Res::Panic => panic!("task panic"),
        }
    }
}

fn new_rt() -> Runtime {
    Runtime::new(Box::new(NoopNotify))
}

fn spawn<F: Future + 'static>(rt: &Runtime, f: F) -> Task<F::Output> {
    unsafe { task::spawn(f, &rt.queue, Location::caller()) }
}

fn locals(rt: &Runtime) -> Vec<Runnable> {
    std::iter::from_fn(|| rt.queue.pop_local()).collect()
}

/// Pops the only local task.
#[track_caller]
fn one_local(rt: &Runtime) -> Runnable {
    flush_references(rt);
    let mut q = locals(rt);
    assert_eq!(q.len(), 1, "local queue");
    q.pop().unwrap()
}

/// Activates a submission or consumes one task's wake notification entries.
#[track_caller]
fn one_remote(rt: &Runtime) -> Option<Runnable> {
    let wakes: Vec<_> = std::iter::from_fn(|| rt.queue.wake_queue.pop()).collect();
    assert!(!wakes.is_empty(), "wake queue");
    let mut runnable = None;
    for task in wakes
        .into_iter()
        .filter_map(|wake| wake.into_runnable(&rt.queue))
    {
        assert!(runnable.replace(task).is_none(), "duplicate run reference");
    }
    runnable
}

#[track_caller]
fn flush_references(rt: &Runtime) {
    while let Some(wake) = rt.queue.wake_queue.pop() {
        assert!(
            wake.into_runnable(&rt.queue).is_none(),
            "unexpected remote wake"
        );
    }
}

#[track_caller]
fn assert_idle(rt: &Runtime) {
    flush_references(rt);
    assert!(rt.queue.local_queue.is_empty(), "local queue");
    assert!(rt.queue.wake_queue.is_empty(), "wake queue");
}

/// Each task allocation owns one weak endpoint.
#[track_caller]
fn assert_freed(rt: &Runtime) {
    assert_idle(rt);
    assert_eq!(rt.queue.task_count(), 0, "task allocations retained");
}

fn join<T>(task: &mut Task<T>) -> Poll<Option<T>> {
    task.poll_join(&Context::from_waker(Waker::noop()))
}

fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    thread::spawn(f).join().unwrap()
}

/// STATEMENT: `Runnable` cannot be sent away from its owner thread.
///
/// HOW:
/// - Resolve a trait method whose implementations would be ambiguous if `Runnable` implemented `Send`.
/// - Require that expression to compile without selecting a `Send` implementation.
#[test]
fn runnable_is_owner_local() {
    trait AmbiguousIfSend<A> {
        fn check() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    struct IsSend;
    impl<T: ?Sized + Send> AmbiguousIfSend<IsSend> for T {}

    let _ = <Runnable as AmbiguousIfSend<_>>::check;
}

/// STATEMENT: Dropping a queued runnable closes its task without polling the future.
///
/// HOW:
/// - Spawn a ready probe and drop its sole local runnable while retaining the task handle.
/// - Check zero polls, one future drop, no output, and a finished handle.
/// - Drop the handle and check empty queues and zero task allocations.
#[test]
fn drop_and_detach() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Ready));
    drop(one_local(&rt));
    p.check(0, 1, 0);
    assert!(task.is_finished());
    drop(task);
    assert_freed(&rt);
}

/// STATEMENT: Dropping a task handle leaves queued work alive until its runnable is released.
///
/// HOW:
/// - Spawn a ready probe and drop its handle; check the future has not been polled or dropped.
/// - Drop the queued runnable and check one unpolled future drop and no retained allocations.
#[test]
fn detach_and_drop() {
    let rt = new_rt();
    let p = Probe::default();
    drop(spawn(&rt, p.future(Res::Ready)));
    p.check(0, 0, 0);
    drop(one_local(&rt));
    p.check(0, 1, 0);
    assert_freed(&rt);
}

/// STATEMENT: A detached ready task runs once and immediately destroys its unclaimed output.
///
/// HOW:
/// - Drop the ready probe's task handle and run its sole local runnable to completion.
/// - Release the saved waker and check one poll, one future drop, one output drop, and no allocations.
#[test]
fn detach_and_run() {
    let rt = new_rt();
    let p = Probe::default();
    drop(spawn(&rt, p.future(Res::Ready)));
    assert!(!one_local(&rt).run());
    drop(p.waker());
    p.check(1, 1, 1);
    assert_freed(&rt);
}

/// STATEMENT: A completed task retains its output until its task handle is dropped.
///
/// HOW:
/// - Run a ready probe and release its waker; check one future drop but no output drop.
/// - Drop the handle and check exactly one output drop and complete allocation reclamation.
#[test]
fn run_and_detach() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Ready));
    one_local(&rt).run();
    drop(p.waker());
    p.check(1, 1, 0);
    drop(task);
    p.check(1, 1, 1);
    assert_freed(&rt);
}

/// STATEMENT: Cancelling queued work defers future destruction to the runner without polling it.
///
/// HOW:
/// - Cancel a ready probe before running it and check all probe counters remain zero.
/// - Run the queued cancellation and check one future drop, no output, and no allocations.
#[test]
fn cancel_and_run() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Ready));
    task.cancel();
    p.check(0, 0, 0);
    one_local(&rt).run();
    p.check(0, 1, 0);
    assert_freed(&rt);
}

/// STATEMENT: Cancelling a completed task destroys its stored output and releases the allocation.
///
/// HOW:
/// - Run a ready probe, release its saved waker, then cancel its handle.
/// - Check exactly one poll, future drop, and output drop, with empty queues and no allocations.
#[test]
fn run_and_cancel() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Ready));
    one_local(&rt).run();
    drop(p.waker());
    task.cancel();
    p.check(1, 1, 1);
    assert_freed(&rt);
}

/// STATEMENT: Dropping or cancelling a finished handle releases its output despite a surviving waker.
///
/// HOW:
/// - Complete a ready probe while retaining its saved waker and check the output is still live.
/// - Drop or cancel the handle, check one output drop, then release the waker and check reclamation.
#[test]
fn drop_finished() {
    for cancel in [false, true] {
        let rt = new_rt();
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Ready));
        one_local(&rt).run();
        p.check(1, 1, 0);
        if cancel {
            task.cancel();
        } else {
            drop(task);
        }
        // a waker keeps the task alive, the output is dropped anyway
        p.check(1, 1, 1);
        drop(p.waker());
        assert_freed(&rt);
    }
}

/// STATEMENT: Handle drop permits queued completion, while cancellation skips the remaining future.
///
/// HOW:
/// - Run a task through its initial yield, then hold its queued runnable.
/// - Drop or cancel its handle before running again and compare probe poll and output-drop counts.
/// - Release any saved waker and check both paths free the future and task allocation.
#[test]
fn drop_and_complete() {
    for cancel in [false, true] {
        let rt = new_rt();
        let p = Probe::default();
        let fut = p.future(Res::Ready);
        let task = spawn(&rt, async move {
            yield_now().await;
            fut.await
        });
        // the first run yields, the task is queued again
        assert!(one_local(&rt).run());
        let r = one_local(&rt);
        if cancel {
            task.cancel();
        } else {
            drop(task);
        }
        r.run();
        let ran = usize::from(!cancel);
        p.check(ran, 1, ran);
        if !cancel {
            drop(p.waker());
        }
        assert_freed(&rt);
    }
}

/// STATEMENT: Detached submissions clean up both completed and unrun futures without a join handle.
///
/// HOW:
/// - Submit ready probes locally and remotely, run once, and check future and output destruction.
/// - Release their wakers and check reclamation; separately drop an unrun detached runnable.
/// - Check that final case drops its future without polling or producing output.
#[test]
fn spawn_detached() {
    for remote in [false, true] {
        let rt = new_rt();
        let p = Probe::default();
        let fut = p.future(Res::Ready);
        let q = rt.queue.clone();
        let spawn =
            move || unsafe { task::spawn_detached::<_, false>(fut, &q, Location::caller()) };
        let r = if remote {
            on_thread(spawn);
            one_remote(&rt).unwrap()
        } else {
            spawn();
            one_local(&rt)
        };
        assert!(!r.run());
        p.check(1, 1, 1);
        drop(p.waker());
        assert_freed(&rt);
    }

    // dropped without a run
    let rt = new_rt();
    let p = Probe::default();
    unsafe { task::spawn_detached::<_, true>(p.future(Res::Ready), &rt.queue, Location::caller()) };
    drop(one_local(&rt));
    p.check(0, 1, 0);
    assert_freed(&rt);
}

/// STATEMENT: A panic in spawn notification aborts rather than unwinding with published work.
///
/// HOW:
/// - Run local and remote detached submissions with a panicking notifier in child processes.
/// - Check failing exits distinct from test-panic code 101 and the spawn-notification diagnostic.
#[test]
#[cfg_attr(miri, ignore = "spawns a process")]
fn spawn_notify_panic_aborts() {
    const CHILD: &str = "NTEX_RT_NOTIFY_PANIC_CHILD";

    #[derive(Debug)]
    struct PanicNotify;

    impl Notify for PanicNotify {
        fn notify(&self) -> io::Result<()> {
            panic!("spawn notification");
        }
    }

    if let Ok(case) = std::env::var(CHILD) {
        let rt = Runtime::new(Box::new(PanicNotify));
        if case == "remote" {
            let handle = rt.handle();
            on_thread(move || handle.spawn_detached(async {}));
        } else {
            super::CURRENT_RUNTIME.set(&rt, || rt.spawn_detached(async {}));
        }
        return;
    }

    for case in ["local", "remote"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["rt::task_tests::spawn_notify_panic_aborts", "--exact"])
            .args(["--nocapture", "--test-threads=1"])
            .env(CHILD, case)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{case}: {stderr}");
        // a normal test failure exits with 101, an abort does not
        assert_ne!(out.status.code(), Some(101), "{case}: {stderr}");
        assert!(stderr.contains("spawn notification"), "{case}: {stderr}");
    }
}
/// STATEMENT: Unshared completion releases outputs and storage without waking detached awaiters.
///
/// HOW:
/// - Complete detached tasks without saved wakers and check immediate output destruction.
/// - Register then drop join handles; check awaiter references release and no awaiter is woken.
/// - Catch a detached output's drop panic and still require empty queues and zero task allocations.
#[test]
fn finish_unshared() {
    struct PanicOnDrop;
    struct PanicOnWake;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("output drop");
        }
    }

    impl std::task::Wake for PanicOnWake {
        fn wake(self: Arc<Self>) {
            panic!("awaiter wake");
        }
    }

    // the run reference is the only one, no waker is kept
    let rt = new_rt();
    let out = Counter::default();
    let tok = out.token();
    let mut tok = Some(tok);
    let fut = poll_fn(move |_| Poll::Ready(tok.take().unwrap()));
    unsafe { task::spawn_detached::<_, true>(fut, &rt.queue, Location::caller()) };
    assert!(!one_local(&rt).run());
    assert_eq!(out.get(), 1);
    assert_freed(&rt);

    // a dropped join handle registered its awaiter
    let rt = new_rt();
    let slot = Arc::new(Mutex::new(None::<Waker>));
    let slot2 = slot.clone();
    let mut tok = Some(out.token());
    let mut polled = false;
    let mut task = spawn(
        &rt,
        poll_fn(move |cx| {
            if polled {
                Poll::Ready(tok.take().unwrap())
            } else {
                polled = true;
                *slot2.lock().unwrap() = Some(cx.waker().clone());
                Poll::Pending
            }
        }),
    );
    one_local(&rt).run();
    let awaiter = Arc::new(CountWaker::default());
    let w = Waker::from(awaiter.clone());
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    drop(task);
    // the owner thread drops the awaiter with the handle
    assert_eq!(Arc::strong_count(&awaiter), 2);
    // the wake passes its reference to the queue
    slot.lock().unwrap().take().unwrap().wake();
    one_local(&rt).run();
    assert_eq!(awaiter.0.load(Ordering::SeqCst), 0);
    assert_eq!(out.get(), 2);
    assert_freed(&rt);
    drop(w);

    // the awaiter of a dropped join handle is not woken
    let rt = new_rt();
    let mut tok = Some(out.token());
    let mut task = spawn(&rt, poll_fn(move |_| Poll::Ready(tok.take().unwrap())));
    let w = Waker::from(Arc::new(PanicOnWake));
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    drop(task);
    assert!(!one_local(&rt).run());
    assert_eq!(out.get(), 3);
    assert_freed(&rt);

    // the task is freed if the output drop panics
    let rt = new_rt();
    let fut = poll_fn(|_| Poll::Ready(PanicOnDrop));
    unsafe { task::spawn_detached::<_, true>(fut, &rt.queue, Location::caller()) };
    let r = one_local(&rt);
    assert!(catch_unwind(AssertUnwindSafe(|| r.run())).is_err());
    assert_freed(&rt);
}

/// STATEMENT: Untouched local completion reclaims directly, while remote clones retain storage safely.
///
/// HOW:
/// - Complete an unshared detached task and check zero allocations and an empty wake FIFO immediately.
/// - Complete tasks retained by a join handle or owner waker, then release those references.
/// - Clone a pending task's waker remotely, complete it locally, release the clone, and check reclamation.
#[test]
fn untouched_completion_reclaims_without_fifo_service() {
    let rt = new_rt();

    // no join handle and no waker
    unsafe { task::spawn_detached::<_, true>(async {}, &rt.queue, Location::caller()) };
    one_local(&rt).run();
    assert_eq!(rt.queue.task_count(), 0);
    assert!(rt.queue.wake_queue.is_empty());
    assert_freed(&rt);

    // a join handle
    let task = spawn(&rt, async {});
    one_local(&rt).run();
    drop(task);

    // a waker on the owner thread
    let slot = Arc::new(Mutex::new(None::<Waker>));
    let slot2 = slot.clone();
    let fut = poll_fn(move |cx| {
        *slot2.lock().unwrap() = Some(cx.waker().clone());
        Poll::Ready(())
    });
    unsafe { task::spawn_detached::<_, true>(fut, &rt.queue, Location::caller()) };
    one_local(&rt).run();
    drop(slot.lock().unwrap().take());

    // a waker cloned on another thread retains the completed allocation
    let slot2 = slot.clone();
    let mut polled = false;
    let fut = poll_fn(move |cx| {
        if polled {
            return Poll::Ready(());
        }
        polled = true;
        *slot2.lock().unwrap() = Some(cx.waker().clone());
        Poll::Pending
    });
    unsafe { task::spawn_detached::<_, true>(fut, &rt.queue, Location::caller()) };
    one_local(&rt).run();
    let w = slot.lock().unwrap().take().unwrap();
    let (w, cloned) = on_thread(move || {
        let cloned = w.clone();
        (w, cloned)
    });
    w.wake();
    one_local(&rt).run();
    drop(cloned);

    assert_freed(&rt);
}

/// STATEMENT: Dropping or cancelling a local join handle releases its awaiter without notification.
///
/// HOW:
/// - Register a counting awaiter after a task yields, then drop or cancel its handle.
/// - Check the awaiter's reference count falls immediately and its wake count stays zero.
/// - Run the held runnable and check completion versus cancellation counters and full reclamation.
#[test]
fn detached_awaiter() {
    // the owner thread drops the awaiter with the join handle
    for cancel in [false, true] {
        let rt = new_rt();
        let p = Probe::default();
        let fut = p.future(Res::Ready);
        let mut task = spawn(&rt, async move {
            yield_now().await;
            fut.await
        });
        assert!(one_local(&rt).run());
        let r = one_local(&rt);
        let awaiter = Arc::new(CountWaker::default());
        let w = Waker::from(awaiter.clone());
        assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
        drop(w);
        assert_eq!(Arc::strong_count(&awaiter), 2);
        if cancel {
            task.cancel();
        } else {
            drop(task);
        }
        assert_eq!(Arc::strong_count(&awaiter), 1);
        r.run();
        let ran = usize::from(!cancel);
        p.check(ran, 1, ran);
        if !cancel {
            drop(p.waker());
        }
        assert_freed(&rt);
        assert_eq!(awaiter.0.load(Ordering::SeqCst), 0);
    }
}

/// STATEMENT: Pending cancellation runs cleanup once, and later wakes of the closed task are inert.
///
/// HOW:
/// - Poll a pending probe and cancel it explicitly or by dropping its opted-in handle.
/// - Check destruction waits until its cancellation runnable runs.
/// - Exercise borrowed and consuming wakes afterward; check no runnable appears and release storage.
#[test]
fn cancel_pending() {
    for cancel_on_drop in [false, true] {
        let rt = new_rt();
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Pending));
        task.set_cancel_on_drop(cancel_on_drop);
        one_local(&rt).run();
        if cancel_on_drop {
            drop(task);
        } else {
            task.cancel();
        }
        p.check(1, 0, 0);
        // the cancel schedules the task, the runner drops the future
        one_local(&rt).run();
        p.check(1, 1, 0);
        // wakers of a closed task do not schedule it
        let w = p.waker();
        w.wake_by_ref();
        w.clone().wake();
        assert_idle(&rt);
        drop(w);
        assert_freed(&rt);
    }
}

/// STATEMENT: Local wakes coalesce until a runnable runs, and the last interest drop schedules cleanup.
///
/// HOW:
/// - Wake a pending task repeatedly before and after removing its runnable; check only one run is queued.
/// - Perform three further wake/run cycles and check five polls with a still-pending join.
/// - Drop the handle and last saved waker, run cleanup, and check one future drop and no allocations.
#[test]
fn schedule() {
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    assert_idle(&rt);

    // wakes coalesce while scheduled
    let w = p.waker();
    w.wake_by_ref();
    w.wake_by_ref();
    w.clone().wake();
    let r = one_local(&rt);
    w.wake_by_ref();
    assert_idle(&rt);
    r.run();
    p.check(2, 0, 0);
    drop(w);

    for _ in 0..3 {
        p.waker().wake();
        one_local(&rt).run();
    }
    p.check(5, 0, 0);
    assert_eq!(join(&mut task).map(|v| v.is_some()), Poll::Pending);
    drop(task);
    // the stored waker keeps the task alive
    assert_idle(&rt);
    drop(p.waker());
    one_local(&rt).run();
    p.check(5, 1, 0);
    assert_freed(&rt);
}

/// STATEMENT: Multiple self-wakes during one poll create only one subsequent runnable.
///
/// HOW:
/// - Detach a future that calls borrowed and consuming wakes in each of its first two polls.
/// - Run exactly three sole local runnables and check two requeues followed by completion.
/// - Check three polls and no remaining task allocations.
#[test]
fn self_wake_during_run() {
    let rt = new_rt();
    let polls = Counter::default();
    let polls2 = polls.clone();
    drop(spawn(
        &rt,
        std::future::poll_fn(move |cx| {
            polls2.inc();
            if polls2.get() < 3 {
                // only the first wake queues the task
                cx.waker().wake_by_ref();
                cx.waker().clone().wake();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }),
    ));
    assert!(one_local(&rt).run());
    assert!(one_local(&rt).run());
    assert!(!one_local(&rt).run());
    assert_eq!(polls.get(), 3);
    assert_freed(&rt);
}

/// STATEMENT: A task's join result is consumed once; subsequent joins return no output.
///
/// HOW:
/// - Check a ready probe's join is pending before running, then returns `Some` after completion.
/// - Check a second join returns `None` and probe counters show one output destruction.
/// - Drop the handle and saved waker and check reclamation.
#[test]
fn join_twice() {
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Ready));
    assert!(join(&mut task).is_pending());
    one_local(&rt).run();
    assert!(matches!(join(&mut task), Poll::Ready(Some(_))));
    p.check(1, 1, 1);
    // the output is consumed
    assert!(matches!(join(&mut task), Poll::Ready(None)));
    drop(task);
    drop(p.waker());
    assert_freed(&rt);
}

/// STATEMENT: Replacing a join waker releases the old one, and cancellation drops the new one silently.
///
/// HOW:
/// - Register two counting awaiters around a pending task's first run and inspect their reference counts.
/// - Cancel the handle, check the second awaiter is released, and run cleanup.
/// - Check neither awaiter was woken and release the probe waker to verify reclamation.
#[test]
fn join_waker_replaced() {
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Pending));
    let a = Arc::new(CountWaker::default());
    let b = Arc::new(CountWaker::default());

    let wa = Waker::from(a.clone());
    assert!(task.poll_join(&Context::from_waker(&wa)).is_pending());
    drop(wa);
    one_local(&rt).run();
    let wb = Waker::from(b.clone());
    assert!(task.poll_join(&Context::from_waker(&wb)).is_pending());
    drop(wb);
    // the replaced waker is released
    assert_eq!(Arc::strong_count(&a), 1);
    assert_eq!(Arc::strong_count(&b), 2);
    // a cancel drops the awaiter without a wake
    task.cancel();
    assert_eq!(Arc::strong_count(&b), 1);
    one_local(&rt).run();
    assert_eq!(a.0.load(Ordering::SeqCst), 0);
    assert_eq!(b.0.load(Ordering::SeqCst), 0);
    drop(p.waker());
    assert_freed(&rt);
}

/// STATEMENT: A native task waker registered as a join awaiter schedules its parent on completion.
///
/// HOW:
/// - Spawn an inner ready probe and an outer task polling the inner join.
/// - Run the outer before the inner, then run the sole outer runnable created by completion.
/// - Check successful join delivery, exact probe drops, and allocation reclamation.
#[test]
fn join_by_task_waker() {
    // a task joining another task registers an ntex waker
    let rt = new_rt();
    let p = Probe::default();
    let mut inner = Some(spawn(&rt, p.future(Res::Ready)));
    let out = Arc::new(Mutex::new(None));
    let out2 = out.clone();
    drop(spawn(
        &rt,
        std::future::poll_fn(move |cx| match inner.as_mut().unwrap().poll_join(cx) {
            Poll::Ready(v) => {
                *out2.lock().unwrap() = Some(v.is_some());
                inner = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }),
    ));
    let mut q = locals(&rt);
    assert_eq!(q.len(), 2);
    let outer = q.pop().unwrap();
    let first = q.pop().unwrap();
    outer.run();
    first.run();
    // the inner task woke the outer one
    one_local(&rt).run();
    assert_eq!(*out.lock().unwrap(), Some(true));
    p.check(1, 1, 1);
    drop(p.waker());
    assert_freed(&rt);
}

/// STATEMENT: Local and remote wakes of completed tasks do not rerun them or retain their outputs.
///
/// HOW:
/// - Complete a probe, drop its handle, and retain only its saved waker.
/// - Exercise borrowed, cloned consuming, and final consuming wakes locally and remotely.
/// - Check probe counters stay at one poll and one drop each, and the allocation is freed.
#[test]
fn wake_completed() {
    let rt = new_rt();
    for remote in [false, true] {
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Ready));
        one_local(&rt).run();
        let w = p.waker();
        drop(task);
        // the output is dropped with the handle, the waker keeps the task
        p.check(1, 1, 1);
        if remote {
            on_thread(move || {
                w.wake_by_ref();
                w.clone().wake();
                w.wake();
            });
            // the last reference frees the task on the waking thread
        } else {
            w.wake_by_ref();
            w.clone().wake();
            assert_idle(&rt);
            w.wake();
        }
        p.check(1, 1, 1);
        assert_freed(&rt);
    }
}

/// STATEMENT: A waker of a cancelled and destroyed future is inert on either thread.
///
/// HOW:
/// - Poll and cancel a pending probe, then run its cleanup and check one future destruction.
/// - Consume the surviving waker locally or remotely and check no extra polls, output, or allocations.
#[test]
fn wake_dropped() {
    let rt = new_rt();
    for remote in [false, true] {
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Pending));
        one_local(&rt).run();
        task.cancel();
        one_local(&rt).run();
        p.check(1, 1, 0);
        let w = p.waker();
        if remote {
            on_thread(move || w.wake());
        } else {
            w.wake();
        }
        p.check(1, 1, 0);
        assert_freed(&rt);
    }
}

/// STATEMENT: Standard wakers surviving completion and shutdown retain storage, not the runtime driver.
///
/// HOW:
/// - Complete a probe, retain two wakers, drop its handle and runtime, and check the queue weak ref expires.
/// - Release or wake those wakers remotely and check exact probe drops and continued queue expiration.
#[test]
fn release_after_shutdown() {
    // Standard wake survivors retain task storage, not the runtime driver.
    for wake in [false, true] {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Ready));
        // The probe waker owns an owner-counted task reference.
        one_local(&rt).run();
        let w = p.waker();
        let w2 = w.clone();
        drop(task);
        drop(rt);
        assert!(queue.upgrade().is_none());
        on_thread(move || {
            if wake {
                w.wake_by_ref();
                w2.wake();
            }
            drop(w);
        });
        p.check(1, 1, 1);
        assert!(queue.upgrade().is_none(), "task alive");
    }
}

/// STATEMENT: Post-shutdown output release is safe even when its own final task waker drops and it panics.
///
/// HOW:
/// - Complete a task whose output holds its own waker and an optional panicking destructor.
/// - Drop the runtime, then drop or cancel the handle while catching any output panic.
/// - Check panic behavior matches the case, one token drops, and the driver queue remains released.
#[test]
fn release_output_after_shutdown() {
    struct Output {
        waker: Option<Waker>,
        panics: bool,
        _drop: Token,
    }

    impl Drop for Output {
        fn drop(&mut self) {
            // The output guard becomes the last reference during destruction.
            drop(self.waker.take());
            assert!(!self.panics, "output drop");
        }
    }

    for cancel in [false, true] {
        for panics in [false, true] {
            let rt = new_rt();
            let queue = Arc::downgrade(&rt.queue);
            let drops = Counter::default();
            let mut token = Some(drops.token());
            let task = spawn(
                &rt,
                poll_fn(move |cx| {
                    Poll::Ready(Output {
                        waker: Some(cx.waker().clone()),
                        panics,
                        _drop: token.take().unwrap(),
                    })
                }),
            );
            one_local(&rt).run();
            drop(rt);
            assert!(queue.upgrade().is_none());
            let panicked = catch_unwind(AssertUnwindSafe(|| {
                if cancel {
                    task.cancel();
                } else {
                    drop(task);
                }
            }))
            .is_err();
            assert_eq!(panicked, panics);
            assert_eq!(drops.get(), 1);
            assert!(queue.upgrade().is_none(), "task alive");
        }
    }
}

/// STATEMENT: Shutdown releases a stale remote wake for a task already completed by a local wake.
///
/// HOW:
/// - Queue a remote wake of a pending task, wake it locally, and complete its local runnable.
/// - Drop its handle and runtime; check the queue weak reference expires despite the queued entry.
/// - Drop the surviving waker remotely and check the queue stays gone.
#[test]
fn queued_then_finished() {
    // queued from another thread, then completed by a local wake
    let rt = new_rt();
    let queue = Arc::downgrade(&rt.queue);
    let slot = Arc::new(Mutex::new(None::<Waker>));
    let slot2 = slot.clone();
    let mut polled = false;
    let task = spawn(
        &rt,
        poll_fn(move |cx| {
            if polled {
                Poll::Ready(())
            } else {
                polled = true;
                *slot2.lock().unwrap() = Some(cx.waker().clone());
                Poll::Pending
            }
        }),
    );
    one_local(&rt).run();
    let w = slot.lock().unwrap().take().unwrap();
    let w2 = w.clone();
    on_thread(move || w2.wake());
    w.wake_by_ref();
    one_local(&rt).run();
    assert!(task.is_finished());
    drop(task);
    // the runtime drops the queued reference
    drop(rt);
    assert!(queue.upgrade().is_none());
    on_thread(move || drop(w));
    assert!(queue.upgrade().is_none(), "task alive");
}

/// STATEMENT: Shutdown destroys pending futures despite surviving handles and standard wakers.
///
/// HOW:
/// - Vary local versus remote submission, handle detachment, and local or remote waker drop or wake.
/// - Poll once, shut down, and check one future drop plus finished, empty joins for surviving handles.
/// - Release the remaining waker and check the driver queue is not retained.
#[test]
fn shutdown_drops_pending() {
    // Shutdown drops pending futures despite surviving standard wakers.
    for remote_spawn in [false, true] {
        for detach in [false, true] {
            for release in 0..4 {
                let rt = new_rt();
                let queue = Arc::downgrade(&rt.queue);
                let p = Probe::default();
                let fut = p.future(Res::Pending);
                let (task, r) = if remote_spawn {
                    let handle = rt.handle();
                    on_thread(move || drop(handle.spawn(fut)));
                    (None, one_remote(&rt).unwrap())
                } else {
                    (Some(spawn(&rt, fut)), one_local(&rt))
                };
                assert!(!r.run());
                let w = p.waker();
                let mut task = if detach {
                    drop(task);
                    None
                } else {
                    task
                };
                drop(rt);
                p.check(1, 1, 0);
                if let Some(task) = &mut task {
                    assert!(task.is_finished());
                    assert!(matches!(join(task), Poll::Ready(None)));
                }
                drop(task);
                assert!(queue.upgrade().is_none());
                match release {
                    0 => drop(w),
                    1 => w.wake(),
                    2 => on_thread(move || drop(w)),
                    _ => on_thread(move || w.wake()),
                }
                assert!(queue.upgrade().is_none(), "task alive");
            }
        }
    }
}

/// STATEMENT: Shutdown cleans reentrantly woken and newly submitted futures even if a destructor panics.
///
/// HOW:
/// - Poll a future whose destructor wakes another pending task and submits an additional probe.
/// - Drop the runtime with and without a destructor panic and check the expected unwind result.
/// - Check the existing task drops once, the new task drops unpolled, handles finish, and the queue frees.
#[test]
fn shutdown_drop_wakes_and_spawns() {
    // a dropped future wakes a pending task and spawns a task, the runtime
    // drops both
    for panics in [false, true] {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let (p1, p2) = (Probe::default(), Probe::default());
        let t1 = spawn(&rt, p1.future(Res::Pending));
        one_local(&rt).run();
        let w1 = p1.waker();
        let q = rt.queue.clone();
        let fut2 = p2.future(Res::Pending);
        let t0 = spawn(
            &rt,
            pending_with_drop(move || {
                w1.wake();
                unsafe { task::spawn_detached::<_, false>(fut2, &q, Location::caller()) };
                assert!(!panics, "reentrant cleanup panic");
            }),
        );
        one_local(&rt).run();
        assert_eq!(catch_unwind(AssertUnwindSafe(|| drop(rt))).is_err(), panics);
        p1.check(1, 1, 0);
        p2.check(0, 1, 0);
        assert!(t0.is_finished() && t1.is_finished());
        drop((t0, t1));
        assert!(queue.upgrade().is_none(), "task alive");
    }
}

/// STATEMENT: A single cleanup panic does not stop shutdown from draining every task source.
///
/// HOW:
/// - Populate local queued work, the pending list, or remote submissions with three drop-tracked futures.
/// - Make one destructor panic, catch its payload, and check all three destructors run on the owner.
/// - Check shutdown closes admission, finishes joins, rejects later local and remote spawns, and frees the queue.
#[test]
fn shutdown_continues_after_panics() {
    // Local queue, pending task list, and remote queue cleanup.
    for path in 0..3 {
        let rt = new_rt();
        let handle = rt.handle();
        let queue = Arc::downgrade(&rt.queue);
        let owner = thread::current().id();
        let drops = Counter::default();
        let panics = Counter::default();
        let mut tasks = Vec::new();
        for panics_on_drop in [true, false, false] {
            let (drops, panics) = (drops.clone(), panics.clone());
            let fut = pending_with_drop(move || {
                assert_eq!(thread::current().id(), owner);
                drops.inc();
                if panics_on_drop {
                    std::panic::panic_any(panics.0.fetch_add(1, Ordering::SeqCst));
                }
            });
            let task = if path == 2 {
                let handle = rt.handle();
                on_thread(move || drop(handle.spawn(fut)));
                None
            } else {
                let task = spawn(&rt, fut);
                if path == 1 {
                    one_local(&rt).run();
                }
                Some(task)
            };
            tasks.push(task);
        }
        let payload = catch_unwind(AssertUnwindSafe(|| drop(rt))).unwrap_err();
        assert_eq!(*payload.downcast::<usize>().unwrap(), 0);
        assert_eq!(drops.get(), 3);
        assert_eq!(panics.get(), 1);
        assert!(handle.queue.is_closed());
        for mut task in tasks.into_iter().flatten() {
            assert!(task.is_finished());
            assert!(matches!(join(&mut task), Poll::Ready(None)));
        }

        for remote in [false, true] {
            let p = Probe::default();
            let (h, fut) = (handle.clone(), p.future(Res::Ready));
            let submit = move || drop(h.spawn(fut));
            if remote {
                on_thread(submit);
            } else {
                submit();
            }
            p.check(0, 1, 0);
        }
        drop(handle);
        assert!(queue.upgrade().is_none(), "task alive");
    }
}

/// STATEMENT: A shutdown join-notification panic still cleans other futures and finishes their handles.
///
/// HOW:
/// - Register a panicking completion waker and queue two drop-tracked pending futures.
/// - Catch runtime-drop's completion panic and check two future drops and one wake callback.
/// - Check both joins finish without output and the queue frees after dropping the handles.
#[test]
fn shutdown_completion_panics() {
    struct PanicWake {
        drops: bool,
        calls: Counter,
    }
    impl std::task::Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            assert!(!self.drops, "detached awaiter was woken");
            self.calls.inc();
            panic!("completion panic");
        }
    }
    impl Drop for PanicWake {
        fn drop(&mut self) {
            if self.drops {
                self.calls.inc();
                panic!("completion panic");
            }
        }
    }

    {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let drops = Counter::default();
        let calls = Counter::default();
        let d = drops.clone();
        let owner = thread::current().id();
        let local = std::rc::Rc::new(());
        let mut task = Some(spawn(
            &rt,
            pending_with_drop(move || {
                assert_eq!(thread::current().id(), owner);
                drop(local);
                d.inc();
            }),
        ));
        let d = drops.clone();
        let mut other = spawn(&rt, pending_with_drop(move || d.inc()));
        let w = Waker::from(Arc::new(PanicWake {
            drops: false,
            calls: calls.clone(),
        }));
        assert!(
            task.as_mut()
                .unwrap()
                .poll_join(&Context::from_waker(&w))
                .is_pending()
        );
        drop(w);
        let payload = catch_unwind(AssertUnwindSafe(|| drop(rt))).unwrap_err();
        assert_eq!(
            payload.downcast_ref::<&str>().copied(),
            Some("completion panic")
        );
        assert_eq!(drops.get(), 2);
        assert_eq!(calls.get(), 1);
        assert!(other.is_finished());
        assert!(matches!(join(&mut other), Poll::Ready(None)));
        if let Some(task) = task.as_mut() {
            assert!(task.is_finished());
            assert!(matches!(join(task), Poll::Ready(None)));
        }
        drop((task, other));
        assert!(queue.upgrade().is_none(), "task alive");
    }
}

/// STATEMENT: A second panic during task cleanup or notification aborts instead of resuming unwinding.
///
/// HOW:
/// - Run child cases for double cleanup, existing unwind, completion wake/drop, poll wake, and output drop.
/// - Check every child fails with a non-101 exit and emits the second-panic diagnostic.
#[test]
#[cfg_attr(miri, ignore = "spawns a process")]
fn shutdown_second_panic_aborts() {
    const CHILD: &str = "NTEX_RT_SECOND_PANIC_CHILD";

    struct PanicWake(bool);
    impl std::task::Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("second panic");
        }
    }
    impl Drop for PanicWake {
        fn drop(&mut self) {
            assert!(!self.0, "second panic");
        }
    }
    struct PanicOutput {
        _waker: Waker,
    }
    impl Drop for PanicOutput {
        fn drop(&mut self) {
            panic!("first panic");
        }
    }

    if let Ok(case) = std::env::var(CHILD) {
        let rt = new_rt();
        match case.as_str() {
            // two cleanup panics
            "cleanup" => {
                drop(spawn(&rt, pending_with_drop(|| panic!("first panic"))));
                drop(spawn(&rt, pending_with_drop(|| panic!("second panic"))));
                drop(rt);
            }
            // a cleanup panic while the runtime is dropped during an unwind
            "unwinding" => {
                drop(spawn(&rt, pending_with_drop(|| panic!("second panic"))));
                let _rt = rt;
                panic!("first panic");
            }
            // the awaiter is notified while unwinding from the future drop
            "completion-wake" | "completion-drop" => {
                let dropped = case == "completion-drop";
                let mut task = spawn(&rt, pending_with_drop(|| panic!("first panic")));
                let w = Waker::from(Arc::new(PanicWake(dropped)));
                assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
                drop(w);
                if dropped {
                    let _drop = OnDrop(Some(|| drop(task)));
                    panic!("first panic");
                }
                drop(rt);
                drop(task);
            }
            // the awaiter is notified while unwinding from the poll
            "poll-wake" => {
                let p = Probe::default();
                let mut task = spawn(&rt, p.future(Res::Panic));
                let w = Waker::from(Arc::new(PanicWake(false)));
                assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
                let _ = catch_unwind(AssertUnwindSafe(|| one_local(&rt).run()));
            }
            // the awaiter is dropped while unwinding from the output drop
            "output-drop" => {
                let w = Waker::from(Arc::new(PanicWake(true)));
                drop(spawn(&rt, async move { PanicOutput { _waker: w } }));
                let _ = catch_unwind(AssertUnwindSafe(|| one_local(&rt).run()));
            }
            _ => unreachable!(),
        }
        return;
    }

    for case in [
        "cleanup",
        "unwinding",
        "completion-wake",
        "completion-drop",
        "poll-wake",
        "output-drop",
    ] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["rt::task_tests::shutdown_second_panic_aborts", "--exact"])
            .args(["--nocapture", "--test-threads=1"])
            .env(CHILD, case)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{case}: {stderr}");
        // a normal test failure exits with 101, an abort does not
        assert_ne!(out.status.code(), Some(101), "{case}: {stderr}");
        assert!(stderr.contains("second panic"), "{case}: {stderr}");
    }
}

/// STATEMENT: Shutdown waits for already-admitted publication even after a local cleanup panic.
///
/// HOW:
/// - Hold a remote entry outside the FIFO with an admission permit and queue a panicking local future.
/// - Publish that entry only after shutdown closes admission, while catching the runtime-drop panic.
/// - Check the original panic payload, both future drops, a finished empty local join, and queue release.
#[test]
fn shutdown_panic_waits_for_admitted_publication() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let rt = new_rt();
    let queue = Arc::downgrade(&rt.queue);
    let drops = Counter::default();
    let d = drops.clone();
    let mut local = spawn(
        &rt,
        pending_with_drop(move || {
            d.inc();
            panic!("first future panic");
        }),
    );
    let (q, d) = (rt.queue.clone(), drops.clone());
    on_thread(move || unsafe {
        task::spawn_detached::<_, false>(
            pending_with_drop(move || d.inc()),
            &q,
            Location::caller(),
        );
    });
    // Hold an admitted entry outside the MPSC queue until admission closes.
    let entry = rt.queue.wake_queue.pop().unwrap().into_remote();
    let (q, (tx, rx)) = (rt.queue.clone(), mpsc::channel());
    let th = thread::spawn(move || {
        let permit = q.admit(false).unwrap();
        tx.send(()).unwrap();
        let start = Instant::now();
        while !q.is_closed() {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "admission stayed open"
            );
            thread::yield_now();
        }
        permit.schedule(entry);
    });
    rx.recv_timeout(Duration::from_secs(30)).unwrap();
    let payload = catch_unwind(AssertUnwindSafe(|| drop(rt))).unwrap_err();
    th.join().unwrap();
    assert_eq!(
        payload.downcast_ref::<&str>().copied(),
        Some("first future panic")
    );
    assert_eq!(drops.get(), 2);
    assert!(local.is_finished());
    assert!(matches!(join(&mut local), Poll::Ready(None)));
    drop(local);
    assert!(queue.upgrade().is_none(), "task alive");
}

/// STATEMENT: Shutdown does not wait on a blocked notifier, which retains its queue until returning.
///
/// HOW:
/// - Block a notifier after an interrupted attempt during remote wake, spawn, or detached spawn.
/// - Drop the runtime while notification is blocked and check one future drop with no extra polling.
/// - Check the queue stays alive until releasing and joining the notifier thread, then expires.
#[test]
fn shutdown_with_blocked_notification() {
    use std::sync::mpsc;
    use std::time::Duration;

    #[derive(Debug)]
    struct BlockingNotify {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        interrupted: AtomicBool,
    }

    impl Notify for BlockingNotify {
        fn notify(&self) -> io::Result<()> {
            if !self.interrupted.swap(true, Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.entered.send(()).unwrap();
            self.release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .expect("shutdown waited for a notifier");
            Ok(())
        }
    }

    struct OwnerDrop {
        owner: thread::ThreadId,
        _local: std::rc::Rc<()>,
    }

    impl Drop for OwnerDrop {
        fn drop(&mut self) {
            assert_eq!(thread::current().id(), self.owner);
        }
    }

    for path in 0..3 {
        let (entered, rx) = mpsc::channel();
        let (tx, release) = mpsc::channel();
        let rt = Runtime::new(Box::new(BlockingNotify {
            entered,
            release: Mutex::new(release),
            interrupted: AtomicBool::new(false),
        }));
        rt.awake();
        let queue = Arc::downgrade(&rt.queue);
        let p = Probe::default();
        let fut = p.future(Res::Pending);
        let th = if path != 0 {
            let handle = rt.handle();
            thread::spawn(move || {
                if path == 1 {
                    handle.spawn(fut);
                } else {
                    handle.spawn_detached(fut);
                }
            })
        } else {
            let owner = OwnerDrop {
                owner: thread::current().id(),
                _local: std::rc::Rc::new(()),
            };
            let task = spawn(&rt, async move {
                let _owner = owner;
                fut.await
            });
            one_local(&rt).run();
            let w = p.waker();
            drop(task);
            thread::spawn(move || w.wake())
        };
        rx.recv_timeout(Duration::from_secs(30)).unwrap();
        drop(rt);
        p.check(usize::from(path == 0), 1, 0);
        // The notifier must retain its queue after the task and runtime are gone.
        assert!(queue.upgrade().is_some());
        tx.send(()).unwrap();
        th.join().unwrap();
        assert!(queue.upgrade().is_none(), "task alive");
    }
}

/// STATEMENT: Submissions after shutdown are destroyed unpolled and any local handle is already closed.
///
/// HOW:
/// - Retain the queue past runtime drop and try local joined, local detached, and remote detached spawns.
/// - Check zero polls, one future drop, and no output for each submission.
/// - Check any returned handle finishes with no result and the queue is released.
#[test]
fn spawn_after_shutdown() {
    // a task spawned after the runtime is dropped is cancelled, its future
    // is dropped on the spawning thread
    for remote in [false, true] {
        for detached in [false, true] {
            let rt = new_rt();
            let q = rt.queue.clone();
            let queue = Arc::downgrade(&rt.queue);
            drop(rt);
            let p = Probe::default();
            let fut = p.future(Res::Ready);
            let task = if remote {
                on_thread(move || unsafe {
                    task::spawn_detached::<_, false>(fut, &q, Location::caller());
                });
                None
            } else if detached {
                unsafe { task::spawn_detached::<_, true>(fut, &q, Location::caller()) };
                drop(q);
                None
            } else {
                let task = unsafe { task::spawn(fut, &q, Location::caller()) };
                drop(q);
                Some(task)
            };
            p.check(0, 1, 0);
            if let Some(mut task) = task {
                assert!(task.is_finished());
                assert!(matches!(join(&mut task), Poll::Ready(None)));
            }
            assert!(queue.upgrade().is_none(), "task alive");
        }
    }
}

/// STATEMENT: Remote wake variants coalesce, merge with local scheduling, and avoid unnecessary runs.
///
/// HOW:
/// - Mix remote borrowed and consuming wakes; check one remote runnable and one additional poll.
/// - Drop a non-final clone remotely and check both queues remain idle.
/// - Combine a local wake with a remote wake, check only the local runnable runs, then release all interest.
/// - Run orphan cleanup and check exact poll/drop counts and no allocations.
#[test]
fn remote_wake_variants() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();

    // wake by ref, clone and wake, wake by value, all coalesce
    let w = p.waker();
    let w = on_thread(move || {
        w.wake_by_ref();
        w.clone().wake();
        let w2 = w.clone();
        w2.wake_by_ref();
        w2.wake();
        w
    });
    one_remote(&rt).unwrap().run();
    p.check(2, 0, 0);
    assert_idle(&rt);

    // Non-final external drops need neither a merge nor a run.
    on_thread(move || drop(w.clone()));
    assert_idle(&rt);
    p.check(2, 0, 0);

    // a remote wake of a locally scheduled task runs it once
    let w = p.waker_clone();
    p.waker().wake();
    on_thread(move || w.wake());
    assert!(one_remote(&rt).is_none());
    let r = one_local(&rt);
    r.run();
    p.check(3, 0, 0);

    drop(task);
    drop(p.waker());
    one_local(&rt).run();
    p.check(3, 1, 0);
    assert_freed(&rt);
}

/// STATEMENT: `LocalWaker` coalesces local wakes, converts to a remote waker, and owns cleanup interest.
///
/// HOW:
/// - Detach a pending task and exercise borrowed/owned conversion, `clone_from`, and coalesced local wakes.
/// - Check `will_wake`, convert one handle to `Waker`, wake remotely, and verify one further poll.
/// - Drop the saved standard waker then the last local waker; run cleanup and check reclamation.
#[test]
fn local_waker_scheduling_and_orphan_cleanup() {
    use crate::LocalWaker;

    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    drop(task);

    let waker = p.waker();
    let mut first = LocalWaker::from_waker(&waker);
    let second = LocalWaker::from(waker);
    first.clone_from(&second);
    first.wake_by_ref();
    second.clone().wake();
    one_local(&rt).run();
    p.check(2, 0, 0);
    assert_idle(&rt);
    assert!(second.will_wake(&p.waker()));

    // Conversion transfers the reference to a real thread-safe waker.
    let remote = first.into_waker();
    on_thread(move || remote.wake());
    one_remote(&rt).unwrap().run();
    p.check(3, 0, 0);
    drop(p.waker());
    assert_idle(&rt);
    drop(second);
    one_local(&rt).run();
    p.check(3, 1, 0);
    assert_freed(&rt);
}

/// STATEMENT: Foreign-thread `LocalWaker` capture uses a working fallback that is inert after shutdown.
///
/// HOW:
/// - Create borrowed and owned local wrappers on a worker and exercise both wake forms and conversion.
/// - Merge the single remote runnable, check the second poll, cancel, and run cleanup.
/// - Shut down and exercise fallback clones and consuming wakes remotely; check the driver queue expires.
#[test]
fn local_waker_foreign_thread_capture() {
    use crate::LocalWaker;

    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    let waker = p.waker();
    let waker = on_thread(move || {
        let borrowed = LocalWaker::from_waker(&waker);
        let owned = LocalWaker::from(waker);
        borrowed.wake_by_ref();
        owned.clone().wake();
        drop(borrowed);
        owned.into_waker()
    });
    one_remote(&rt).unwrap().run();
    p.check(2, 0, 0);
    drop(p.waker());
    task.cancel();
    one_local(&rt).run();
    // The foreign-thread fallback also remains usable after shutdown.
    let queue = Arc::downgrade(&rt.queue);
    drop(rt);
    on_thread(move || {
        let local = LocalWaker::from(waker);
        local.clone().wake();
        local.wake();
    });
    p.check(2, 1, 0);
    assert!(queue.upgrade().is_none());
}

/// STATEMENT: `LocalWaker` remains safe after completion, panic, or shutdown without retaining the driver.
///
/// HOW:
/// - Save a local waker from an owner-bound future and vary pending shutdown, ready completion, and panic.
/// - Check expected panic behavior, the ready `Rc` result, and exactly one owner-thread future drop.
/// - After shutdown, exercise local wakes, standard conversions, and remote releases; check queue expiration.
#[test]
fn local_waker_completion_and_shutdown() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use crate::LocalWaker;

    for result in [Res::Pending, Res::Ready, Res::Panic] {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let slot = Rc::new(RefCell::new(None));
        let saved = slot.clone();
        let ready = Rc::new(Cell::new(false));
        let complete = ready.clone();
        let drops = Counter::default();
        let token = drops.token();
        let owner = thread::current().id();
        let hook = OnDrop(Some(move || {
            assert_eq!(thread::current().id(), owner);
            drop(token);
        }));
        let mut task = spawn(&rt, async move {
            let _hook = hook;
            poll_fn(move |cx| {
                if !complete.get() {
                    *saved.borrow_mut() = Some(LocalWaker::from_waker(cx.waker()));
                    return Poll::Pending;
                }
                assert_ne!(result, Res::Panic, "task panic");
                Poll::Ready(Rc::new(42))
            })
            .await
        });
        one_local(&rt).run();
        let local = slot.borrow_mut().take().unwrap();
        if result != Res::Pending {
            ready.set(true);
            local.wake_by_ref();
            let ran = catch_unwind(AssertUnwindSafe(|| one_local(&rt).run()));
            assert_eq!(ran.is_err(), result == Res::Panic);
            if result == Res::Ready {
                let Poll::Ready(Some(value)) = join(&mut task) else {
                    panic!("missing output");
                };
                assert_eq!(*value, 42);
            }
        }
        drop(rt);
        drop(task);
        assert_eq!(drops.get(), 1);
        local.wake_by_ref();
        local.clone().wake();
        // Acquiring another local handle after completion uses shared counts.
        let standard = local.clone().into_waker();
        LocalWaker::from_waker(&standard).wake();
        LocalWaker::from(standard).wake();
        let remote = local.into_waker();
        on_thread(move || {
            remote.clone().wake();
            drop(remote);
        });
        assert!(queue.upgrade().is_none(), "task alive");
    }
}

/// STATEMENT: Dropping a detached pending task's final waker schedules destruction, not another poll.
///
/// HOW:
/// - Poll and detach a probe, then drop its saved waker locally or remotely.
/// - Merge any remote release, run the sole cleanup runnable, and check one poll and one future drop.
/// - Require empty queues and zero task allocations.
#[test]
fn drop_last_waker() {
    let rt = new_rt();
    for remote in [false, true] {
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Pending));
        one_local(&rt).run();
        drop(task);
        p.check(1, 0, 0);

        let w = p.waker();
        if remote {
            on_thread(move || drop(w));
            assert!(one_remote(&rt).is_none());
        } else {
            drop(w);
        }
        // nothing can wake the task, it is scheduled to drop the future
        one_local(&rt).run();
        p.check(1, 1, 0);
        assert_freed(&rt);
    }
}

/// STATEMENT: Releasing the last task handle without any waker schedules pending-future cleanup.
///
/// HOW:
/// - Poll a pending probe, release its saved waker, and check the runtime is idle.
/// - Drop or cancel the remaining task handle and run the sole cleanup runnable.
/// - Check the future was polled once, dropped once, and its allocation freed.
#[test]
fn drop_last_task() {
    let rt = new_rt();
    for cancel in [false, true] {
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Pending));
        one_local(&rt).run();
        drop(p.waker());
        p.check(1, 0, 0);
        assert_idle(&rt);

        if cancel {
            task.cancel();
        } else {
            drop(task);
        }
        one_local(&rt).run();
        p.check(1, 1, 0);
        assert_freed(&rt);
    }
}

/// STATEMENT: Merging a remote wake during a running poll preserves one follow-up local run.
///
/// HOW:
/// - Wake the task remotely inside its first poll and consume the FIFO entry reentrantly.
/// - Check that merge produces no immediate runnable, then run the next local runnable to completion.
/// - Check exactly two polls, a successful join, and no remaining allocations.
#[test]
fn remote_wake_merged_while_running() {
    let rt = new_rt();
    let queue = rt.queue.clone();
    let polls = Counter::default();
    let polls2 = polls.clone();
    let mut task = spawn(
        &rt,
        poll_fn(move |cx| {
            polls2.inc();
            if polls2.get() > 1 {
                return Poll::Ready(());
            }
            let w = cx.waker().clone();
            on_thread(move || w.wake());
            // a reentrant runtime poll merges the wake while the task runs
            let remote = queue.wake_queue.pop().unwrap();
            assert!(remote.into_runnable(&queue).is_none());
            Poll::Pending
        }),
    );
    one_local(&rt).run();
    one_local(&rt).run();
    assert_eq!(polls.get(), 2);
    assert_eq!(join(&mut task), Poll::Ready(Some(())));
    drop(task);
    assert_freed(&rt);
}

/// STATEMENT: Remote submission activates through the wake queue and releases its detached result.
///
/// HOW:
/// - Spawn a ready probe remotely and drop its remote join handle.
/// - Check the local queue is empty, activate the sole remote runnable, and run it once.
/// - Check one poll and one future/output drop, then release the saved waker and verify reclamation.
#[test]
fn remote_spawn() {
    let rt = new_rt();
    let p = Probe::default();
    let handle = rt.handle();
    let fut = p.future(Res::Ready);
    on_thread(move || drop(handle.spawn(fut)));
    assert!(rt.queue.local_queue.is_empty());
    one_remote(&rt).unwrap().run();
    p.check(1, 1, 1);
    drop(p.waker());
    p.check(1, 1, 1);
    assert_freed(&rt);
}

/// STATEMENT: Shutdown destroys a remote submission before activation without polling it.
///
/// HOW:
/// - Remotely spawn and detach a ready probe, retain the queue, and drop the runtime without running work.
/// - Check zero polls, one future drop, no output, and zero remaining task allocations.
#[test]
fn remote_spawn_shutdown() {
    let rt = new_rt();
    let p = Probe::default();
    let handle = rt.handle();
    let fut = p.future(Res::Ready);
    on_thread(move || drop(handle.spawn(fut)));
    let queue = rt.queue.clone();
    drop(rt);
    p.check(0, 1, 0);
    assert_eq!(queue.task_count(), 0);
}

/// Runs the task on the owner thread while `during` runs on another thread
/// in the middle of the poll. Returns whether the poll panicked.
fn during_run<T: Send + 'static>(
    r: Runnable,
    gate: &Arc<Barrier>,
    during: impl FnOnce() -> T + Send + 'static,
) -> (bool, T) {
    let gate2 = gate.clone();
    let th = thread::spawn(move || {
        gate2.wait();
        let res = during();
        gate2.wait();
        res
    });
    let res = catch_unwind(AssertUnwindSafe(|| r.run()));
    (res.is_err(), th.join().unwrap())
}

/// Spawns a task with a gated future and runs it to the gated poll.
fn gated_task(rt: &Runtime, p: &Probe, res: Res, gate: &Arc<Barrier>) -> (Task<Token>, Runnable) {
    let fut = p.gated(res, gate);
    let task = spawn(rt, async move {
        // the first run returns before the gated poll
        yield_now().await;
        fut.await
    });
    assert!(one_local(rt).run());
    (task, one_local(rt))
}

async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

/// STATEMENT: A remote wake during poll queues remotely and is discarded if that poll panics.
///
/// HOW:
/// - Pause pending and panicking probe polls at a barrier and consume their wakers on another thread.
/// - Check the owner local queue stays empty and panic status matches the future's result mode.
/// - Merge the wake; rerun only the pending future, then check one final future drop and reclamation.
#[test]
fn wake_during_run() {
    for res in [Res::Pending, Res::Panic] {
        let rt = new_rt();
        let p = Probe::default();
        let gate = Arc::new(Barrier::new(2));
        let (task, r) = gated_task(&rt, &p, res, &gate);
        let p2 = p.clone();
        let (panicked, ()) = during_run(r, &gate, move || p2.waker().wake());
        assert_eq!(panicked, res == Res::Panic);
        // a remote wake is queued remotely, never locally
        assert!(rt.queue.local_queue.is_empty());
        let r = one_remote(&rt);
        if res == Res::Pending {
            p.check(1, 0, 0);
            r.unwrap().run();
            p.check(2, 0, 0);
            drop(task);
            drop(p.waker());
            one_local(&rt).run();
        } else {
            assert!(r.is_none());
            drop(task);
        }
        p.check(p.polls.get(), 1, 0);
        assert_freed(&rt);
    }
}

/// STATEMENT: A poll panic closes the join without output and makes saved wakers inert.
///
/// HOW:
/// - Catch a probe's poll panic and check one poll, one future drop, and a finished empty join.
/// - Consume the saved waker, check no new work appears, and release the handle to verify reclamation.
#[test]
fn panic_and_join() {
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Panic));
    let r = one_local(&rt);
    assert!(catch_unwind(AssertUnwindSafe(|| r.run())).is_err());
    p.check(1, 1, 0);
    assert!(task.is_finished());
    assert!(matches!(join(&mut task), Poll::Ready(None)));
    // wakers of a panicked task do not schedule it
    p.waker().wake();
    assert_idle(&rt);
    drop(task);
    assert_freed(&rt);
}

/// STATEMENT: A panicking completion waker cannot lose the result or leak the run reference.
///
/// HOW:
/// - Register a panicking join waker and catch its panic while running a ready probe.
/// - Retrieve the result anyway, check exact future/output drops, and release all references.
/// - Require empty queues and zero task allocations.
#[test]
fn panicking_completion_waker() {
    struct PanicWake;
    impl std::task::Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("completion waker panic");
        }
    }
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Ready));
    let w = Waker::from(Arc::new(PanicWake));
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    assert!(catch_unwind(AssertUnwindSafe(|| one_local(&rt).run())).is_err());
    // the run reference is released before the awaiter is woken
    assert!(matches!(join(&mut task), Poll::Ready(Some(_))));
    p.check(1, 1, 1);
    drop(task);
    drop(p.waker());
    assert_freed(&rt);
}

/// STATEMENT: A panic dropping a detached awaiter leaves queued completion and output cleanup intact.
///
/// HOW:
/// - Register a waker that panics on drop and must never be woken, leaving its last reference in the handle.
/// - Catch the panic from dropping the task handle, then run the detached task.
/// - Check its output drops once and all task storage is freed.
#[test]
fn panicking_detached_awaiter_drop() {
    struct PanicDrop;
    impl std::task::Wake for PanicDrop {
        fn wake(self: Arc<Self>) {
            unreachable!("the awaiter of a dropped join handle is woken");
        }
    }
    impl Drop for PanicDrop {
        fn drop(&mut self) {
            panic!("completion waker drop panic");
        }
    }
    let rt = new_rt();
    let outputs = Counter::default();
    let output = outputs.token();
    let mut task = spawn(&rt, async move { output });
    let w = Waker::from(Arc::new(PanicDrop));
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    drop(w);
    assert!(catch_unwind(AssertUnwindSafe(|| drop(task))).is_err());
    one_local(&rt).run();
    assert_eq!(outputs.get(), 1);
    assert_freed(&rt);
}

fn panic_clone_waker() -> Waker {
    static PANIC: std::task::RawWakerVTable =
        std::task::RawWakerVTable::new(|_| panic!("waker clone panic"), |_| (), |_| (), |_| ());
    unsafe { Waker::from_raw(std::task::RawWaker::new(std::ptr::null(), &PANIC)) }
}

/// STATEMENT: A join-waker clone panic preserves the previously registered awaiter.
///
/// HOW:
/// - Register a counting waker, then catch a replacement waker's clone panic.
/// - Poll again with the original waker, run the ready task, and check that awaiter is notified once.
/// - Retrieve the output and release the handle and saved task waker to check reclamation.
#[test]
fn panicking_local_registration() {
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Ready));
    let awaiter = Arc::new(CountWaker::default());
    let w = Waker::from(awaiter.clone());
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    let bad = panic_clone_waker();
    let res = catch_unwind(AssertUnwindSafe(|| {
        task.poll_join(&Context::from_waker(&bad))
    }));
    assert!(res.is_err());
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    one_local(&rt).run();
    // The original awaiter survives a failed replacement.
    assert_eq!(awaiter.0.load(Ordering::SeqCst), 1);
    assert!(matches!(join(&mut task), Poll::Ready(Some(_))));
    drop(task);
    drop(p.waker());
    assert_freed(&rt);
}

/// STATEMENT: Shutdown merges local and remote references without retaining the runnable queue.
///
/// HOW:
/// - Poll and detach a pending probe, clone its waker remotely, and issue remote and local wakes.
/// - Release the remaining waker, retain the queue, and drop the runtime.
/// - Check one poll and one future drop, and only the retained queue reference remains.
#[test]
fn runtime_drop_merges_remote_refs() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    let w = p.waker();
    drop(task);
    let w2 = on_thread(move || {
        let w2 = w.clone();
        w.wake();
        w2
    });
    // queued locally and remotely
    w2.wake_by_ref();
    drop(w2);
    let q = rt.queue.clone();
    drop(rt);
    p.check(1, 1, 0);
    assert_eq!(Arc::strong_count(&q), 1, "tasks alive");
}

struct Slot {
    waker: Mutex<Option<Waker>>,
    // polls left until ready
    left: AtomicUsize,
}

struct StressFut {
    slots: Arc<Vec<Slot>>,
    idx: usize,
    outputs: Counter,
    created: Counter,
    _drop: Token,
}

impl Future for StressFut {
    type Output = Token;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Token> {
        let slot = &self.slots[self.idx];
        *slot.waker.lock().unwrap() = Some(cx.waker().clone());
        let left = slot.left.fetch_sub(1, Ordering::Relaxed);
        if left <= 1 {
            self.created.inc();
            Poll::Ready(self.outputs.token())
        } else {
            if left.is_multiple_of(3) {
                cx.waker().wake_by_ref();
            }
            Poll::Pending
        }
    }
}

/// xorshift64, returns a value in `0..n`
fn rand(s: &mut u64, n: usize) -> usize {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    usize::try_from(*s % n as u64).unwrap()
}

fn slot_waker(slots: &[Slot], idx: usize) -> Option<Waker> {
    slots[idx].waker.lock().unwrap().clone()
}

/// Submits detached futures and exercises their standard wakers remotely.
fn stress_worker(mut rng: u64, slots: &[Slot], queue: &Arc<RunnableQueue>, futs: Vec<StressFut>) {
    let mut indices = Vec::new();
    for f in futs {
        indices.push(f.idx);
        unsafe {
            task::spawn_detached::<_, false>(f, queue, Location::caller());
        }
    }
    for idx in indices {
        for _ in 0..rand(&mut rng, 4) {
            if let Some(w) = slot_waker(slots, rand(&mut rng, slots.len())) {
                match rand(&mut rng, 3) {
                    0 => w.wake_by_ref(),
                    1 => w.clone().wake(),
                    _ => drop(w.clone()),
                }
            }
        }
        while slots[idx].left.load(Ordering::Relaxed) != 0 {
            if let Some(w) = slot_waker(slots, idx) {
                w.wake();
            }
            thread::yield_now();
        }
    }
}

/// Services bounded scheduler turns until `done` and all queues are empty.
fn run_until_done(rt: &Runtime, done: impl Fn() -> bool) {
    loop {
        let finished = done();
        let again = rt.poll() == crate::PollResult::PollAgain;
        if finished && !again {
            break;
        }
        if !again {
            thread::yield_now();
        }
    }
}

/// STATEMENT: Mixed local joins, remote detached submissions, and concurrent wakes reclaim every task.
///
/// HOW:
/// - Spawn half the futures locally and half on workers that vary remote wake and clone/drop operations.
/// - Cancel or detach local handles, service bounded turns, and wait for every worker to finish.
/// - Release stored wakers and drain; check all futures drop and output drops match outputs created.
/// - Require empty queues and zero allocations after every round.
///
/// `NTEX_RAW_STRESS=<rounds>` runs more rounds.
#[test]
fn stress() {
    const WORKERS: usize = if cfg!(miri) { 2 } else { 4 };
    let (rounds, per_worker) = if cfg!(miri) {
        (1, 2)
    } else {
        let rounds = std::env::var("NTEX_RAW_STRESS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20u64);
        (rounds, 32)
    };
    let rt = new_rt();

    for round in 0..rounds {
        let total = WORKERS * per_worker * 2;
        let slots: Arc<Vec<Slot>> = Arc::new(
            (0..total)
                .map(|i| Slot {
                    waker: Mutex::new(None),
                    left: AtomicUsize::new(1 + i % 7),
                })
                .collect(),
        );
        let (drops, outputs, created) =
            (Counter::default(), Counter::default(), Counter::default());
        let fut = |idx| StressFut {
            slots: slots.clone(),
            idx,
            outputs: outputs.clone(),
            created: created.clone(),
            _drop: drops.token(),
        };

        // the first half is spawned on the owner thread, the rest remotely
        let mut handles = Vec::new();
        for idx in 0..total / 2 {
            handles.push(spawn(&rt, fut(idx)));
        }
        let done = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..WORKERS)
            .map(|w| {
                let futs = (0..per_worker)
                    .map(|i| fut(total / 2 + w * per_worker + i))
                    .collect();
                let (slots, done, q) = (slots.clone(), done.clone(), rt.queue.clone());
                let rng = (round + 1) * 0x9E37_79B9 + w as u64 * 7 + 1;
                thread::spawn(move || {
                    stress_worker(rng, &slots, &q, futs);
                    done.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();

        for (i, task) in handles.into_iter().enumerate() {
            if i.is_multiple_of(2) {
                task.cancel();
            } else {
                drop(task);
            }
        }
        run_until_done(&rt, || done.load(Ordering::SeqCst) == WORKERS);
        for w in workers {
            w.join().unwrap();
        }

        // release the stored wakers, detached pending tasks get closed
        for slot in slots.iter() {
            drop(slot.waker.lock().unwrap().take());
        }
        run_until_done(&rt, || true);
        assert_eq!(drops.get(), total, "futures dropped");
        assert_eq!(outputs.get(), created.get(), "outputs dropped");
        assert_freed(&rt);
    }
}

/// STATEMENT: Remote waker operations and submissions do not allocate an owner thread index.
///
/// HOW:
/// - Move a task waker and runtime handle to a fresh thread and exercise wakes, clones, and both spawn forms.
/// - Check `try_current_thread_id` remains `None`, then release the task and runtime.
#[test]
#[cfg(not(ntex_loom))]
fn remote_use_assigns_no_thread_index() {
    let rt = new_rt();
    let handle = rt.handle();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    let w = p.waker_clone();
    let assigned = on_thread(move || {
        w.wake_by_ref();
        drop(w.clone());
        drop(w);
        handle.spawn_detached(async {});
        drop(handle.spawn(async {}));
        super::try_current_thread_id()
    });
    assert_eq!(assigned, None);
    drop(task);
    drop(rt);
}

/// STATEMENT: Runtime and owner-thread identifier exhaustion aborts instead of reusing an ID.
///
/// HOW:
/// - In child processes, set each index just below `u16::MAX` and issue the final valid ID.
/// - Attempt one more allocation, using a fresh thread for owner-thread exhaustion.
/// - Check a non-101 failure, the final-ID marker, and absence of the post-exhaustion marker.
#[test]
#[cfg(not(ntex_loom))]
#[cfg_attr(miri, ignore = "spawns a process")]
fn id_exhaustion_aborts() {
    const CHILD: &str = "NTEX_RT_ID_EXHAUSTION_CHILD";

    if let Ok(case) = std::env::var(CHILD) {
        if case == "runtime" {
            crate::rt::wake::set_runtime_index(u16::MAX - 1);
            let _rt = new_rt();
            eprintln!("last id issued");
            let _rt = new_rt();
        } else {
            super::set_thread_index(u16::MAX - 1);
            let _rt = new_rt();
            eprintln!("last id issued");
            on_thread(|| drop(new_rt()));
        }
        eprintln!("ids not exhausted");
        return;
    }

    for case in ["runtime", "thread"] {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["rt::task_tests::id_exhaustion_aborts", "--exact"])
            .args(["--nocapture", "--test-threads=1"])
            .env(CHILD, case)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{case}: {stderr}");
        // a normal test failure exits with 101, an abort does not
        assert_ne!(out.status.code(), Some(101), "{case}: {stderr}");
        assert!(stderr.contains("last id issued"), "{case}: {stderr}");
        assert!(!stderr.contains("ids not exhausted"), "{case}: {stderr}");
    }
}
