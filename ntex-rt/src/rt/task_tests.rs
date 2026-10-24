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
use crate::rt::task::{self, Remote, Runnable, Task};

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
    unsafe { task::spawn::<_, true>(f, &rt.queue, Location::caller()) }
}

fn locals(rt: &Runtime) -> Vec<Runnable> {
    std::iter::from_fn(|| rt.queue.pop_local()).collect()
}

fn remotes(rt: &Runtime) -> Vec<Remote> {
    std::iter::from_fn(|| rt.queue.remote_queue.pop().unwrap()).collect()
}

/// Pops the only local task.
#[track_caller]
fn one_local(rt: &Runtime) -> Runnable {
    let mut q = locals(rt);
    assert_eq!(q.len(), 1, "local queue");
    q.pop().unwrap()
}

/// Pops the only remote task and merges it.
#[track_caller]
fn one_remote(rt: &Runtime) -> Option<Runnable> {
    let mut q = remotes(rt);
    assert_eq!(q.len(), 1, "remote queue");
    q.pop().unwrap().into_runnable()
}

#[track_caller]
fn assert_idle(rt: &Runtime) {
    assert!(rt.queue.local_queue.is_empty(), "local queue");
    assert!(rt.queue.remote_queue.is_empty(), "remote queue");
}

/// Every task header keeps the runtime queue alive.
#[track_caller]
fn assert_freed(rt: &Runtime) {
    assert_idle(rt);
    assert_eq!(Arc::strong_count(&rt.queue), 1, "tasks alive");
}

fn join<T>(task: &mut Task<T>) -> Poll<Option<T>> {
    task.poll_join(&Context::from_waker(Waker::noop()))
}

fn on_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    thread::spawn(f).join().unwrap()
}

#[test]
#[cfg(debug_assertions)]
#[cfg_attr(miri, ignore = "leaks the task")]
fn run_off_owner_thread() {
    let rt = new_rt();
    let task = spawn(&rt, async {});
    let r = one_local(&rt);
    // the runnable is dropped on the other thread, the task leaks
    assert!(thread::spawn(move || r.run()).join().is_err());
    drop(task);
}

#[test]
fn drop_and_detach() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Ready));
    drop(one_local(&rt));
    p.check(0, 1, 0);
    assert!(task.is_finished());
    task.detach();
    assert_freed(&rt);
}

#[test]
fn detach_and_drop() {
    let rt = new_rt();
    let p = Probe::default();
    spawn(&rt, p.future(Res::Ready)).detach();
    p.check(0, 0, 0);
    drop(one_local(&rt));
    p.check(0, 1, 0);
    assert_freed(&rt);
}

#[test]
fn detach_and_run() {
    let rt = new_rt();
    let p = Probe::default();
    spawn(&rt, p.future(Res::Ready)).detach();
    assert!(!one_local(&rt).run());
    drop(p.waker());
    p.check(1, 1, 1);
    assert_freed(&rt);
}

#[test]
fn run_and_detach() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Ready));
    one_local(&rt).run();
    drop(p.waker());
    p.check(1, 1, 0);
    task.detach();
    p.check(1, 1, 1);
    assert_freed(&rt);
}

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

#[test]
fn drop_finished() {
    for remote in [false, true] {
        let rt = new_rt();
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Ready));
        one_local(&rt).run();
        p.check(1, 1, 0);
        if remote {
            // a finished task is released without the owner thread
            on_thread(move || drop(task));
            assert_idle(&rt);
        } else {
            drop(task);
        }
        // a waker keeps the task alive, the output is dropped anyway
        p.check(1, 1, 1);
        drop(p.waker());
        assert_freed(&rt);
    }
}

#[test]
fn drop_and_complete() {
    for (remote, cancel) in [(false, false), (true, false), (false, true), (true, true)] {
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
        let detach = move || {
            if cancel {
                task.cancel();
            } else {
                drop(task);
            }
        };
        if remote {
            on_thread(detach);
            // a remote cancel queues the task, the release is merged
            if let Some(r) = one_remote(&rt) {
                assert!(cancel);
                r.run();
            }
        } else {
            detach();
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

#[test]
fn finish_unshared_fast_path() {
    // the completion is not published only if the run reference is the last
    let fast = || task::UNSHARED_FINISHES.with(std::cell::Cell::get);
    let rt = new_rt();
    let n = fast();

    // no join handle and no waker
    unsafe { task::spawn_detached::<_, true>(async {}, &rt.queue, Location::caller()) };
    one_local(&rt).run();
    assert_eq!(fast(), n + 1);

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

    // a waker cloned on another thread, counted in the remote word
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

    assert_eq!(fast(), n + 1);
    assert_freed(&rt);
}

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

    // a join handle dropped on another thread leaves the awaiter, the
    // completion drops it without a wake
    for merge_first in [false, true] {
        let rt = new_rt();
        let p = Probe::default();
        let fut = p.future(Res::Ready);
        let task = spawn(&rt, async move {
            yield_now().await;
            fut.await
        });
        assert!(one_local(&rt).run());
        let r = one_local(&rt);
        let awaiter = Arc::new(CountWaker::default());
        let a2 = awaiter.clone();
        on_thread(move || {
            let mut task = task;
            let w = Waker::from(a2);
            assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
        });
        assert_eq!(Arc::strong_count(&awaiter), 2);
        let merge = || {
            for m in remotes(&rt) {
                assert!(m.into_runnable().is_none());
            }
        };
        if merge_first {
            merge();
        }
        r.run();
        if !merge_first {
            merge();
        }
        assert_eq!(awaiter.0.load(Ordering::SeqCst), 0);
        assert_eq!(Arc::strong_count(&awaiter), 1);
        p.check(1, 1, 1);
        drop(p.waker());
        assert_freed(&rt);
    }
}

#[test]
fn cancel_pending() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    task.cancel();
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

#[test]
fn self_wake_during_run() {
    let rt = new_rt();
    let polls = Counter::default();
    let polls2 = polls.clone();
    spawn(
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
    )
    .detach();
    assert!(one_local(&rt).run());
    assert!(one_local(&rt).run());
    assert!(!one_local(&rt).run());
    assert_eq!(polls.get(), 3);
    assert_freed(&rt);
}

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

#[test]
fn join_by_task_waker() {
    // a task joining another task registers an ntex waker
    let rt = new_rt();
    let p = Probe::default();
    let mut inner = Some(spawn(&rt, p.future(Res::Ready)));
    let out = Arc::new(Mutex::new(None));
    let out2 = out.clone();
    spawn(
        &rt,
        std::future::poll_fn(move |cx| match inner.as_mut().unwrap().poll_join(cx) {
            Poll::Ready(v) => {
                *out2.lock().unwrap() = Some(v.is_some());
                inner = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }),
    )
    .detach();
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

#[test]
fn release_after_shutdown() {
    // the last reference of a finished task, released on another thread
    // after the runtime is dropped, frees the task and the runtime queue
    for wake in [false, true] {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Ready));
        // the probe waker is cloned while the task runs, a local reference
        one_local(&rt).run();
        let w = p.waker();
        let w2 = w.clone();
        drop(task);
        drop(rt);
        assert!(queue.upgrade().is_some());
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

    for remote in [false, true] {
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
            assert!(queue.upgrade().is_some());
            let release = move || catch_unwind(AssertUnwindSafe(|| drop(task))).is_err();
            let panicked = if remote { on_thread(release) } else { release() };
            assert_eq!(panicked, panics);
            assert_eq!(drops.get(), 1);
            assert!(queue.upgrade().is_none(), "task alive");
        }
    }
}

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
    assert!(queue.upgrade().is_some());
    on_thread(move || drop(w));
    assert!(queue.upgrade().is_none(), "task alive");
}

#[test]
fn shutdown_drops_pending() {
    // a task waiting on a waker is dropped with the runtime, the waker
    // released after the runtime frees the task and the runtime queue
    for remote_spawn in [false, true] {
        for detach in [false, true] {
            for release in 0..4 {
                let rt = new_rt();
                let queue = Arc::downgrade(&rt.queue);
                let p = Probe::default();
                let fut = p.future(Res::Pending);
                let q = rt.queue.clone();
                let spawn = move || unsafe { task::spawn::<_, false>(fut, &q, Location::caller()) };
                let (task, r) = if remote_spawn {
                    let task = on_thread(spawn);
                    (task, one_remote(&rt).unwrap())
                } else {
                    (spawn(), one_local(&rt))
                };
                assert!(!r.run());
                let w = p.waker();
                let mut task = (!detach).then_some(task);
                drop(rt);
                p.check(1, 1, 0);
                if let Some(task) = &mut task {
                    assert!(task.is_finished());
                    assert!(matches!(join(task), Poll::Ready(None)));
                }
                drop(task);
                assert!(queue.upgrade().is_some());
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
                let queue = rt.queue.clone();
                on_thread(move || unsafe {
                    task::spawn::<_, false>(fut, &queue, Location::caller())
                })
            } else {
                let task = spawn(&rt, fut);
                if path == 1 {
                    one_local(&rt).run();
                }
                task
            };
            tasks.push(task);
        }
        let payload = catch_unwind(AssertUnwindSafe(|| drop(rt))).unwrap_err();
        assert_eq!(*payload.downcast::<usize>().unwrap(), 0);
        assert_eq!(drops.get(), 3);
        assert_eq!(panics.get(), 1);
        assert!(handle.queue.is_closed());
        for mut task in tasks {
            assert!(task.is_finished());
            assert!(matches!(join(&mut task), Poll::Ready(None)));
        }

        for remote in [false, true] {
            let p = Probe::default();
            let (h, fut) = (handle.clone(), p.future(Res::Ready));
            let submit = move || h.spawn(fut);
            let task = if remote { on_thread(submit) } else { submit() };
            assert!(task.is_finished());
            p.check(0, 1, 0);
        }
        drop(handle);
        assert!(queue.upgrade().is_none(), "task alive");
    }
}

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

    for drop_waker in [false, true] {
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
            drops: drop_waker,
            calls: calls.clone(),
        }));
        assert!(
            task.as_mut()
                .unwrap()
                .poll_join(&Context::from_waker(&w))
                .is_pending()
        );
        drop(w);
        if drop_waker {
            let task = task.take().unwrap();
            on_thread(move || drop(task));
        }
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
    struct PanicOutput;
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
                spawn(&rt, pending_with_drop(|| panic!("first panic"))).detach();
                spawn(&rt, pending_with_drop(|| panic!("second panic"))).detach();
                drop(rt);
            }
            // a cleanup panic while the runtime is dropped during an unwind
            "unwinding" => {
                spawn(&rt, pending_with_drop(|| panic!("second panic"))).detach();
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
                    on_thread(move || drop(task));
                    drop(rt);
                } else {
                    drop(rt);
                    drop(task);
                }
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
                let mut task = spawn(&rt, async { PanicOutput });
                let w = Waker::from(Arc::new(PanicWake(true)));
                assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
                on_thread(move || drop(task));
                drop(w);
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
    let mut remote = on_thread(move || unsafe {
        task::spawn::<_, false>(pending_with_drop(move || d.inc()), &q, Location::caller())
    });
    // Hold an admitted entry outside the MPSC queue until admission closes.
    let entry = remotes(&rt).pop().unwrap();
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
    for task in [&mut local, &mut remote] {
        assert!(task.is_finished());
        assert!(matches!(join(task), Poll::Ready(None)));
    }
    drop((local, remote));
    assert!(queue.upgrade().is_none(), "task alive");
}

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
                    drop(handle.spawn(fut));
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
            let spawn = move || {
                if detached {
                    unsafe { task::spawn_detached::<_, false>(fut, &q, Location::caller()) };
                    None
                } else {
                    Some(unsafe { task::spawn::<_, false>(fut, &q, Location::caller()) })
                }
            };
            let task = if remote { on_thread(spawn) } else { spawn() };
            p.check(0, 1, 0);
            if let Some(mut task) = task {
                assert!(task.is_finished());
                assert!(matches!(join(&mut task), Poll::Ready(None)));
            }
            assert!(queue.upgrade().is_none(), "task alive");
        }
    }
}

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

    // clones dropped remotely are merged without a run
    on_thread(move || drop(w.clone()));
    assert!(one_remote(&rt).is_none());
    p.check(2, 0, 0);

    // a remote wake of a locally scheduled task runs it once
    let w = p.waker_clone();
    p.waker().wake();
    on_thread(move || w.wake());
    let r = one_local(&rt);
    assert!(one_remote(&rt).is_none());
    r.run();
    p.check(3, 0, 0);

    drop(task);
    drop(p.waker());
    one_local(&rt).run();
    p.check(3, 1, 0);
    assert_freed(&rt);
}

#[test]
fn local_waker_scheduling_and_orphan_cleanup() {
    use crate::LocalWaker;

    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    task.detach();

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

#[test]
fn drop_last_waker() {
    let rt = new_rt();
    for remote in [false, true] {
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Pending));
        one_local(&rt).run();
        task.detach();
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

#[test]
fn drop_last_task() {
    let rt = new_rt();
    for (remote, cancel) in [(false, false), (false, true), (true, false), (true, true)] {
        let p = Probe::default();
        let task = spawn(&rt, p.future(Res::Pending));
        one_local(&rt).run();
        drop(p.waker());
        p.check(1, 0, 0);
        assert_idle(&rt);

        if remote {
            on_thread(move || {
                if cancel {
                    task.cancel();
                } else {
                    task.detach();
                }
            });
            let r = one_remote(&rt);
            // a cancel runs the task, the last release schedules it locally
            assert_eq!(r.is_some(), cancel);
            r.unwrap_or_else(|| one_local(&rt)).run();
        } else {
            if cancel {
                task.cancel();
            } else {
                task.detach();
            }
            one_local(&rt).run();
        }
        p.check(1, 1, 0);
        assert_freed(&rt);
    }
}

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
            let remote = queue.remote_queue.pop().unwrap().unwrap();
            assert!(remote.into_runnable().is_none());
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

#[test]
fn remote_spawn() {
    let rt = new_rt();
    let p = Probe::default();
    let q = rt.queue.clone();
    let fut = p.future(Res::Ready);
    let mut task =
        on_thread(move || unsafe { task::spawn::<_, false>(fut, &q, Location::caller()) });
    assert!(rt.queue.local_queue.is_empty());
    one_remote(&rt).unwrap().run();
    p.check(1, 1, 0);
    assert!(matches!(join(&mut task), Poll::Ready(Some(_))));
    drop(task);
    drop(p.waker());
    p.check(1, 1, 1);
    assert_freed(&rt);
}

#[test]
fn remote_spawn_cancel() {
    let rt = new_rt();
    let p = Probe::default();
    let q = rt.queue.clone();
    let fut = p.future(Res::Ready);
    on_thread(move || unsafe { task::spawn::<_, false>(fut, &q, Location::caller()) }.cancel());
    // queued once, the runner drops the future
    one_remote(&rt).unwrap().run();
    p.check(0, 1, 0);
    assert_freed(&rt);
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

#[test]
fn cancel_during_run() {
    // cancel only, wake then cancel, cancel then wake
    for res in [Res::Pending, Res::Ready, Res::Panic] {
        for order in 0..3 {
            let rt = new_rt();
            let p = Probe::default();
            let gate = Arc::new(Barrier::new(2));
            let (task, r) = gated_task(&rt, &p, res, &gate);
            let p2 = p.clone();
            let (panicked, ()) = during_run(r, &gate, move || {
                let w = p2.waker_clone();
                if order == 1 {
                    w.wake_by_ref();
                }
                task.cancel();
                if order == 2 {
                    w.wake_by_ref();
                }
                // the future is dropped on the owner thread
                assert_eq!(p2.drops.get(), 0);
            });
            assert_eq!(panicked, res == Res::Panic, "{res:?} {order}");
            assert!(rt.queue.local_queue.is_empty());
            // the output is dropped on completion, a waker is still alive
            p.check(
                1,
                usize::from(res != Res::Pending),
                usize::from(res == Res::Ready),
            );
            // queued once, a pending future is dropped without a poll
            if let Some(r) = one_remote(&rt) {
                assert_eq!(res, Res::Pending);
                r.run();
            }
            drop(p.waker());
            p.check(1, 1, usize::from(res == Res::Ready));
            assert_freed(&rt);
        }
    }
}

#[test]
fn detach_during_run() {
    for res in [Res::Pending, Res::Ready, Res::Panic] {
        let rt = new_rt();
        let p = Probe::default();
        let gate = Arc::new(Barrier::new(2));
        let (task, r) = gated_task(&rt, &p, res, &gate);
        let (panicked, ()) = during_run(r, &gate, move || task.detach());
        assert_eq!(panicked, res == Res::Panic);
        // the release is merged without a run
        assert!(one_remote(&rt).is_none());
        p.check(
            1,
            usize::from(res != Res::Pending),
            usize::from(res == Res::Ready),
        );
        drop(p.waker());
        if res == Res::Pending {
            one_local(&rt).run();
        }
        p.check(1, 1, usize::from(res == Res::Ready));
        assert_freed(&rt);
    }
}

#[test]
fn join_during_run() {
    for res in [Res::Ready, Res::Panic] {
        let rt = new_rt();
        let p = Probe::default();
        let gate = Arc::new(Barrier::new(2));
        let (task, r) = gated_task(&rt, &p, res, &gate);
        let woken = Arc::new(CountWaker::default());
        let woken2 = woken.clone();
        let (panicked, mut task) = during_run(r, &gate, move || {
            let mut task = task;
            let w = Waker::from(woken2);
            assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
            task
        });
        assert_eq!(panicked, res == Res::Panic);
        // completion wakes the awaiter once
        assert_eq!(woken.0.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&woken), 1);
        let (ready, task) = on_thread(move || {
            let ready = matches!(join(&mut task), Poll::Ready(Some(_)));
            (ready, task)
        });
        assert_eq!(ready, res == Res::Ready);
        p.check(1, 1, usize::from(res == Res::Ready));
        drop(task);
        drop(p.waker());
        assert_freed(&rt);
    }
}

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

#[test]
fn panicking_completion_waker_drop() {
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
    // the owner thread drops the awaiter of a remotely dropped join handle
    on_thread(move || drop(task));
    assert!(catch_unwind(AssertUnwindSafe(|| one_local(&rt).run())).is_err());
    assert_eq!(outputs.get(), 1);
    assert!(one_remote(&rt).is_none());
    assert_freed(&rt);
}

/// A waker whose clone panics, the clone waits twice on `GATE` first.
fn panic_clone_waker(gate: bool) -> Waker {
    static GATED: std::task::RawWakerVTable = std::task::RawWakerVTable::new(
        |_| {
            GATE.wait();
            GATE.wait();
            panic!("waker clone panic")
        },
        |_| (),
        |_| (),
        |_| (),
    );
    static PANIC: std::task::RawWakerVTable =
        std::task::RawWakerVTable::new(|_| panic!("waker clone panic"), |_| (), |_| (), |_| ());
    let vtable = if gate { &GATED } else { &PANIC };
    unsafe { Waker::from_raw(std::task::RawWaker::new(std::ptr::null(), vtable)) }
}

static GATE: Barrier = Barrier::new(2);

#[test]
fn panicking_remote_registration() {
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Ready));
    let awaiter = Arc::new(CountWaker::default());
    let w = Waker::from(awaiter.clone());
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    let mut task = on_thread(move || {
        let bad = panic_clone_waker(false);
        let res = catch_unwind(AssertUnwindSafe(|| {
            task.poll_join(&Context::from_waker(&bad))
        }));
        assert!(res.is_err());
        task
    });
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    one_local(&rt).run();
    // the registration is cleared, the completion wakes the awaiter
    assert_eq!(awaiter.0.load(Ordering::SeqCst), 1);
    assert!(matches!(join(&mut task), Poll::Ready(Some(_))));
    drop(task);
    drop(p.waker());
    assert_freed(&rt);
}

#[test]
fn panicking_remote_registration_completed() {
    let rt = new_rt();
    let p = Probe::default();
    let mut task = spawn(&rt, p.future(Res::Ready));
    let awaiter = Arc::new(CountWaker::default());
    let w = Waker::from(awaiter.clone());
    assert!(task.poll_join(&Context::from_waker(&w)).is_pending());
    let remote = thread::spawn(move || {
        let bad = panic_clone_waker(true);
        let res = catch_unwind(AssertUnwindSafe(|| {
            task.poll_join(&Context::from_waker(&bad))
        }));
        assert!(res.is_err());
        // completed while registering, the output is ready
        assert!(matches!(join(&mut task), Poll::Ready(Some(_))));
    });
    GATE.wait();
    // the completion skips the awaiter of the registering join handle
    one_local(&rt).run();
    GATE.wait();
    remote.join().unwrap();
    assert_eq!(awaiter.0.load(Ordering::SeqCst), 0);
    p.check(1, 1, 1);
    drop(p.waker());
    assert_freed(&rt);
}

#[test]
fn runtime_drop_merges_remote_refs() {
    let rt = new_rt();
    let p = Probe::default();
    let task = spawn(&rt, p.future(Res::Pending));
    one_local(&rt).run();
    let w = p.waker();
    let w2 = on_thread(move || {
        let w2 = w.clone();
        w.wake();
        drop(task);
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

/// Spawns `futs` remotely, then pokes random wakers and joins, cancels or
/// detaches each task.
fn stress_worker(
    mut rng: u64,
    slots: &[Slot],
    queue: &Arc<RunnableQueue>,
    mut tasks: Vec<(usize, Task<Token>)>,
    futs: Vec<StressFut>,
) {
    for f in futs {
        let idx = f.idx;
        tasks.push((idx, unsafe {
            task::spawn::<_, false>(f, queue, Location::caller())
        }));
    }
    for (idx, mut task) in tasks {
        for _ in 0..rand(&mut rng, 4) {
            if let Some(w) = slot_waker(slots, rand(&mut rng, slots.len())) {
                match rand(&mut rng, 3) {
                    0 => w.wake_by_ref(),
                    1 => w.clone().wake(),
                    _ => drop(w.clone()),
                }
            }
        }
        match rand(&mut rng, 4) {
            0 => task.cancel(),
            1 => drop(task),
            _ => {
                let waker = Waker::from(Arc::new(CountWaker::default()));
                let cx = Context::from_waker(&waker);
                loop {
                    if let Poll::Ready(out) = task.poll_join(&cx) {
                        assert!(out.is_some(), "task {idx} is not cancelled");
                        break;
                    }
                    if let Some(w) = slot_waker(slots, idx) {
                        w.wake();
                    }
                    thread::yield_now();
                }
            }
        }
    }
}

/// Runs local and remote tasks until `done` and both queues are empty.
fn run_until_done(rt: &Runtime, done: impl Fn() -> bool) {
    loop {
        let finished = done();
        let mut ran = false;
        while let Some(r) = rt.queue.pop_local() {
            r.run();
            ran = true;
        }
        loop {
            match rt.queue.remote_queue.pop() {
                Ok(Some(r)) => {
                    if let Some(r) = r.into_runnable() {
                        r.run();
                    }
                    ran = true;
                }
                Ok(None) => break,
                Err(_) => thread::yield_now(),
            }
        }
        if finished && !ran {
            break;
        }
        if !ran {
            thread::yield_now();
        }
    }
}

/// Wakes, clones and drops wakers, joins, cancels and detaches tasks from
/// several threads while the owner thread runs them.
///
/// `NTEX_RAW_STRESS=<rounds>` runs more rounds.
#[test]
fn stress() {
    const WORKERS: usize = 4;
    let (rounds, per_worker) = if cfg!(miri) {
        (1, 4)
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
        let mut handles: Vec<Vec<_>> = (0..WORKERS).map(|_| Vec::new()).collect();
        for idx in 0..total / 2 {
            handles[idx % WORKERS].push((idx, spawn(&rt, fut(idx))));
        }
        let done = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = handles
            .into_iter()
            .enumerate()
            .map(|(w, tasks)| {
                let futs = (0..per_worker)
                    .map(|i| fut(total / 2 + w * per_worker + i))
                    .collect();
                let (slots, done, q) = (slots.clone(), done.clone(), rt.queue.clone());
                let rng = (round + 1) * 0x9E37_79B9 + w as u64 * 7 + 1;
                thread::spawn(move || {
                    stress_worker(rng, &slots, &q, tasks, futs);
                    done.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect();

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
