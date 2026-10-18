//! Loom models of the raw task protocol between the owner thread and other
//! threads: the remote word, the remote queue, the join handle awaiter and
//! the output hand-off.
//!
//! `RUSTFLAGS="--cfg ntex_rt_task --cfg ntex_loom" cargo test --release -p ntex-rt --lib raw_loom`
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use loom::sync::atomic::{AtomicBool, AtomicUsize};
use loom::thread;

use super::{Arc, Future, Location, Notify, Ordering, Runtime, io};
use crate::raw::{self, Task};

#[derive(Debug)]
struct NoopNotify;

impl Notify for NoopNotify {
    fn notify(&self) -> io::Result<()> {
        Ok(())
    }
}

/// Future state shared with the test.
#[derive(Default)]
struct State {
    polls: AtomicUsize,
    drops: AtomicUsize,
    outputs: AtomicUsize,
    // completes the future on the next poll
    ready: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl State {
    fn waker(&self) -> Waker {
        self.waker.lock().unwrap().clone().unwrap()
    }
}

struct Fut(Arc<State>);

impl Future for Fut {
    type Output = Box<usize>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Box<usize>> {
        self.0.polls.fetch_add(1, Ordering::Relaxed);
        if self.0.ready.load(Ordering::Acquire) {
            Poll::Ready(Box::new(42))
        } else {
            *self.0.waker.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Drop for Fut {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// Wakes by setting a flag.
#[derive(Default)]
struct Flag(AtomicBool);

impl std::task::Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

fn new_rt() -> Runtime {
    Runtime::new(Box::new(NoopNotify))
}

fn spawn<F: Future>(rt: &Runtime, f: F) -> Task<F::Output> {
    unsafe { raw::spawn(f, rt.queue.clone(), Location::caller()) }
}

/// Runs queued tasks until both queues are empty.
fn drain(rt: &Runtime) {
    loop {
        if let Some(r) = rt.queue.pop_local() {
            r.run();
            continue;
        }
        match rt.queue.remote_queue.pop() {
            Ok(Some(r)) => {
                if let Some(r) = r.into_runnable() {
                    r.run();
                }
            }
            Ok(None) => return,
            // a producer is between its swap and its link
            Err(_) => thread::yield_now(),
        }
    }
}

/// Runs queued tasks until `done` returns true.
fn run_until(rt: &Runtime, done: impl Fn() -> bool) {
    loop {
        drain(rt);
        if done() {
            return;
        }
        thread::yield_now();
    }
}

/// Polls the join handle until it is ready.
fn join<T>(mut task: Task<T>) -> Option<T> {
    let flag = Arc::new(Flag::default());
    let waker = Waker::from(flag.clone());
    loop {
        if let Poll::Ready(v) = task.poll_join(&Context::from_waker(&waker)) {
            return v;
        }
        // loom only detects spin loops on plain loads, not on RMW ops
        while !flag.0.load(Ordering::Acquire) {
            thread::yield_now();
        }
        flag.0.store(false, Ordering::Relaxed);
    }
}

#[track_caller]
fn assert_freed(rt: &Runtime) {
    drain(rt);
    assert!(rt.queue.local_queue.is_empty(), "local queue");
    assert!(rt.queue.remote_queue.is_empty(), "remote queue");
    assert_eq!(Arc::strong_count(&rt.queue), 1, "tasks alive");
}

/// Spawns a pending task and polls it once.
fn pending(rt: &Runtime) -> (Arc<State>, Task<Box<usize>>) {
    let st = Arc::new(State::default());
    let task = spawn(rt, Fut(st.clone()));
    drain(rt);
    assert_eq!(st.polls.load(Ordering::Relaxed), 1);
    (st, task)
}

fn model(f: impl Fn() + Sync + Send + 'static) {
    let mut b = loom::model::Builder::new();
    if b.preemption_bound.is_none() {
        b.preemption_bound = Some(3);
    }
    b.check(f);
}

/// A remote wake is not lost while the owner thread runs the task.
#[test]
fn remote_wake() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let w = st.waker();
        let st2 = st.clone();
        let th = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake();
        });
        run_until(&rt, || task.is_finished());
        th.join().unwrap();
        assert_eq!(*join(task).unwrap(), 42);
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
    });
}

/// Two threads wake two tasks, the remote queue has two producers.
#[test]
fn two_producers() {
    model(|| {
        let rt = new_rt();
        let (st1, t1) = pending(&rt);
        let (st2, t2) = pending(&rt);
        let ths: Vec<_> = [&st1, &st2]
            .into_iter()
            .map(|st| {
                let (st, w) = (st.clone(), st.waker());
                thread::spawn(move || {
                    st.ready.store(true, Ordering::Release);
                    w.wake();
                })
            })
            .collect();
        run_until(&rt, || t1.is_finished() && t2.is_finished());
        for th in ths {
            th.join().unwrap();
        }
        drop((t1, t2));
        for st in [st1, st2] {
            drop(st.waker.lock().unwrap().take());
            assert_eq!(st.drops.load(Ordering::Relaxed), 1);
        }
        assert_freed(&rt);
    });
}

/// The join handle on another thread registers while the owner thread
/// completes the task, and reads the output the owner thread wrote.
#[test]
fn remote_join() {
    model(|| {
        let rt = new_rt();
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let task = spawn(&rt, Fut(st.clone()));
        let th = thread::spawn(move || *join(task).unwrap());
        drain(&rt);
        assert_eq!(th.join().unwrap(), 42);
        assert_freed(&rt);
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
    });
}

/// The join handle on another thread registers while a remote wake
/// completes the task.
#[test]
fn remote_join_and_wake() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let (st2, w) = (st.clone(), st.waker());
        let waker = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake();
        });
        let joiner = thread::spawn(move || *join(task).unwrap());
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        waker.join().unwrap();
        assert_eq!(joiner.join().unwrap(), 42);
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
    });
}

/// Another thread cancels the task while it is woken and run.
#[test]
fn remote_cancel() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let w = st.waker();
        let th = thread::spawn(move || {
            w.wake_by_ref();
            task.cancel();
            drop(w);
        });
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        th.join().unwrap();
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
        assert!(st.polls.load(Ordering::Relaxed) <= 2);
    });
}

/// The last references are released on both threads at once.
#[test]
fn remote_release() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let w = st.waker.lock().unwrap().take().unwrap();
        let st2 = st.clone();
        let th = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake_by_ref();
            drop(w);
        });
        drop(task);
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        th.join().unwrap();
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
    });
}

/// A completed output is released while another thread clones and drops
/// the remaining waker, after the runtime is gone.
#[test]
fn completed_output_release() {
    model(|| {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let st = Arc::new(State::default());
        let (fut, st2) = (Fut(st.clone()), st.clone());
        let task = spawn(&rt, async move {
            let _ = fut.await;
            Out(st2)
        });
        drain(&rt);
        let w = st.waker.lock().unwrap().take().unwrap();
        st.ready.store(true, Ordering::Release);
        w.wake_by_ref();
        drain(&rt);
        assert!(task.is_finished());
        drop(rt);
        let th = thread::spawn(move || {
            let cloned = w.clone();
            drop(w);
            drop(cloned);
        });
        drop(task);
        th.join().unwrap();
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
        assert_eq!(st.outputs.load(Ordering::Relaxed), 1);
        assert!(queue.upgrade().is_none(), "task alive");
    });
}

/// A task spawned from another thread is run and joined there.
#[test]
fn remote_spawn() {
    model(|| {
        let rt = new_rt();
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let (q, fut) = (rt.queue.clone(), Fut(st.clone()));
        let th = thread::spawn(move || {
            let task = unsafe { raw::spawn(fut, q, Location::caller()) };
            *join(task).unwrap()
        });
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        assert_eq!(th.join().unwrap(), 42);
        assert_freed(&rt);
    });
}

/// Drops the output, counts on the future state.
struct Out(Arc<State>);

impl Drop for Out {
    fn drop(&mut self) {
        self.0.outputs.fetch_add(1, Ordering::Relaxed);
    }
}

/// The join handle is dropped on another thread while the owner thread
/// completes the task, the output is dropped once.
#[test]
fn remote_detach() {
    model(|| {
        let rt = new_rt();
        let st = Arc::new(State::default());
        let (fut, st2) = (Fut(st.clone()), st.clone());
        let task = spawn(&rt, async move {
            let _ = fut.await;
            Out(st2)
        });
        drain(&rt);
        st.ready.store(true, Ordering::Release);
        st.waker().wake();
        let th = thread::spawn(move || drop(task));
        drain(&rt);
        th.join().unwrap();
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
        assert_eq!(st.outputs.load(Ordering::Relaxed), 1);
    });
}

/// A detached task spawned from another thread, the owner thread may free
/// it as soon as it is queued.
#[test]
fn remote_spawn_detached() {
    model(|| {
        let rt = new_rt();
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let (q, fut, st2) = (rt.queue.clone(), Fut(st.clone()), st.clone());
        let th = thread::spawn(move || unsafe {
            raw::spawn_detached(
                async move {
                    let _ = fut.await;
                    Out(st2)
                },
                q,
                Location::caller(),
            );
        });
        run_until(&rt, || st.outputs.load(Ordering::Relaxed) == 1);
        th.join().unwrap();
        assert_freed(&rt);
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
    });
}
