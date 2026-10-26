//! Loom models of the task protocol between the owner thread and other
//! threads: owner-accounted references, detached submissions, local joins,
//! scheduler shutdown and parked-driver notifications. The command FIFO uses
//! a Loom mutex here; these models do not explore Crossbeam's internals.
//!
//! `RUSTFLAGS="--cfg ntex_loom" cargo test --release -p ntex-rt --lib task_loom`
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use loom::sync::atomic::{AtomicBool, AtomicUsize};
use loom::thread;

use super::{Arc, Future, Location, Notify, Ordering, PollResult, Runtime, io};
use crate::rt::task::{self, Task};

#[derive(Debug)]
struct NoopNotify;

impl Notify for NoopNotify {
    fn notify(&self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
struct ParkNotify(Arc<loom::sync::Notify>);

impl Notify for ParkNotify {
    fn notify(&self) -> io::Result<()> {
        self.0.notify();
        Ok(())
    }
}

/// Unlike `drain`, this driver relies on notifications once it is idle.
fn run_parked(rt: &Runtime, park: &ParkNotify, done: impl Fn() -> bool) {
    loop {
        let result = rt.poll();
        if done() {
            return;
        }
        match result {
            PollResult::Pending => {
                // A notification preceding the wait leaves a retained wake token.
                park.0.wait();
                rt.awake();
            }
            PollResult::PollAgain => thread::yield_now(),
            PollResult::Ready => panic!("runtime stopped before completing its tasks"),
        }
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
    unsafe { task::spawn(f, &rt.queue, Location::caller()) }
}

/// Runs queued tasks until both queues are empty.
fn drain(rt: &Runtime) {
    loop {
        if let Some(r) = rt.queue.pop_local() {
            r.run();
            continue;
        }
        let Some(wake) = rt.queue.wake_queue.pop() else {
            return;
        };
        if let Some(r) = wake.into_runnable(&rt.queue) {
            r.run();
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
    assert!(rt.queue.wake_queue.is_empty(), "wake queue");
    assert_eq!(rt.queue.task_count(), 0, "task allocations retained");
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

#[test]
fn consuming_wake_races_last_local_interest() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let waker = st.waker.lock().unwrap().take().unwrap();
        let local = crate::LocalWaker::from_waker(&waker);
        drop(task);
        let state = st.clone();
        let producer = thread::spawn(move || {
            state.ready.store(true, Ordering::Relaxed);
            waker.wake();
        });
        drop(local);
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        producer.join().unwrap();
        assert_eq!(st.polls.load(Ordering::Relaxed), 2);
        assert_freed(&rt);
    });
}

#[test]
fn remote_acquisition_precedes_owner_return_and_release() {
    model(|| {
        let rt = new_rt();
        let (state, task) = pending(&rt);
        let source = state.waker.lock().unwrap().take().unwrap();
        let producer = thread::spawn(move || {
            let cloned = source.clone();
            (source, cloned)
        });
        drop(task);
        drain(&rt);
        let (source, cloned) = producer.join().unwrap();
        drop((source, cloned));
        assert_freed(&rt);
        assert_eq!(state.drops.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn first_remote_publication_races_completed_owner_release() {
    model(|| {
        for operation in 0..3 {
            let rt = new_rt();
            let mut task = spawn(
                &rt,
                std::future::poll_fn(|cx| Poll::Ready(cx.waker().clone())),
            );
            drain(&rt);
            let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop()))
            else {
                panic!("missing completed waker");
            };
            let producer = thread::spawn(move || match operation {
                0 => {
                    let cloned = waker.clone();
                    (Some(waker), Some(cloned))
                }
                1 => {
                    waker.wake_by_ref();
                    (Some(waker), None)
                }
                _ => {
                    waker.wake();
                    (None, None)
                }
            });
            drop(task);
            drain(&rt);
            let returned = producer.join().unwrap();
            drop(returned);
            assert_freed(&rt);
        }
    });
}

#[test]
fn owner_release_follows_pending_remote_releases() {
    model(|| {
        let rt = new_rt();
        let (state, task) = pending(&rt);
        let source = state.waker.lock().unwrap().take().unwrap();
        let first = source.clone();
        let second = source.clone();
        drop(task);
        drain(&rt);
        let producer = thread::spawn(move || {
            drop((first, second));
            let cloned = source.clone();
            (source, cloned)
        });
        let (source, cloned) = producer.join().unwrap();
        drop(source);
        drain(&rt);
        assert_eq!(state.drops.load(Ordering::Relaxed), 0);
        drop(cloned);
        assert_freed(&rt);
        assert_eq!(state.drops.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn clone_racing_unregister_leaves_no_driver_reference() {
    model(|| {
        let rt = new_rt();
        let (state, task) = pending(&rt);
        let source = state.waker.lock().unwrap().take().unwrap();
        let queue = Arc::downgrade(&rt.queue);
        let native = source.vtable();
        let producer = thread::spawn(move || {
            let cloned = source.clone();
            source.wake();
            cloned
        });
        drop(rt);
        let late = producer.join().unwrap();
        assert!(!std::ptr::eq(late.clone().vtable(), native));
        late.wake();
        drop(task);
        assert_eq!(state.drops.load(Ordering::Relaxed), 1);
        assert!(queue.upgrade().is_none());
    });
}

#[test]
fn cached_remote_clone_races_shutdown() {
    model(|| {
        let rt = new_rt();
        let (state, task) = pending(&rt);
        let source = state.waker.lock().unwrap().take().unwrap();
        let queue = Arc::downgrade(&rt.queue);
        let native = source.vtable();
        let (primed, ready) = loom::sync::mpsc::channel();
        let producer = thread::spawn(move || {
            drop(source.clone());
            primed.send(()).unwrap();
            let cloned = source.clone();
            source.wake();
            cloned
        });
        ready.recv().unwrap();
        drop(rt);
        let late = producer.join().unwrap();
        assert!(!std::ptr::eq(late.clone().vtable(), native));
        late.wake();
        drop(task);
        assert_eq!(state.drops.load(Ordering::Relaxed), 1);
        assert!(queue.upgrade().is_none());
    });
}

#[test]
fn cached_owner_endpoint_is_invalidated_independently() {
    model(|| {
        let first = new_rt();
        let second = new_rt();
        let (first_state, first_task) = pending(&first);
        let (second_state, second_task) = pending(&second);
        let first_waker = first_state.waker.lock().unwrap().take().unwrap();
        let second_waker = second_state.waker.lock().unwrap().take().unwrap();
        let native = first_waker.vtable();
        let first_local = crate::LocalWaker::from(first_waker);
        let second_local = crate::LocalWaker::from(second_waker);
        drop(first_local.clone().into_waker());
        drop(second_local.clone().into_waker());
        let remote = second_local.clone().into_waker();
        let remote = thread::spawn(move || {
            drop(remote.clone());
            remote
        })
        .join()
        .unwrap();
        assert!(!second.queue.wake_queue.is_empty());
        drop(remote);
        drain(&first);
        drain(&second);
        drop(first);
        assert!(std::ptr::eq(
            second_local.clone().into_waker().vtable(),
            native
        ));
        assert!(!std::ptr::eq(
            first_local.clone().into_waker().vtable(),
            native
        ));
        drop(second);
        assert!(!std::ptr::eq(
            second_local.clone().into_waker().vtable(),
            native
        ));
        drop((first_local, first_task, second_local, second_task));
        assert_eq!(first_state.drops.load(Ordering::Relaxed), 1);
        assert_eq!(second_state.drops.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn coalesced_wake_publishes_after_queue_push() {
    struct Data(loom::cell::UnsafeCell<usize>);
    unsafe impl Sync for Data {}

    model(|| {
        let rt = new_rt();
        let data = Arc::new(Data(loom::cell::UnsafeCell::new(0)));
        let written = Arc::new(AtomicBool::new(false));
        let consumed = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(None));
        let (s, value) = (slot.clone(), data.clone());
        let mut first = true;
        let mut task = spawn(
            &rt,
            std::future::poll_fn(move |cx| {
                if first {
                    first = false;
                    *s.lock().unwrap() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(value.0.with(|p| unsafe { *p }))
                }
            }),
        );
        drain(&rt);
        let waker = slot.lock().unwrap().take().unwrap();
        let waker = thread::spawn(move || {
            waker.wake_by_ref();
            waker
        })
        .join()
        .unwrap();
        let (w, c) = (written.clone(), consumed.clone());
        let producer = thread::spawn(move || {
            data.0.with_mut(|p| unsafe { *p = 42 });
            waker.wake_by_ref();
            // No synchronization through this flag: the wake must publish data.
            w.store(true, Ordering::Relaxed);
            while !c.load(Ordering::Acquire) {
                thread::yield_now();
            }
            drop(waker);
        });
        while !written.load(Ordering::Relaxed) {
            thread::yield_now();
        }
        drain(&rt);
        assert_eq!(
            task.poll_join(&Context::from_waker(Waker::noop())),
            Poll::Ready(Some(42))
        );
        consumed.store(true, Ordering::Release);
        producer.join().unwrap();
        drop(task);
        assert_freed(&rt);
    });
}

#[test]
fn local_waker_completion_and_remote_release() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let local = crate::LocalWaker::from(st.waker());
        let remote = local.clone().into_waker();
        let th = thread::spawn(move || {
            let cloned = remote.clone();
            let fallback = crate::LocalWaker::from_waker(&remote);
            fallback.clone().wake();
            drop(fallback);
            drop(cloned);
            remote.wake();
        });
        st.ready.store(true, Ordering::Release);
        local.clone().wake();
        drain(&rt);
        assert_eq!(*join(task).unwrap(), 42);
        local.clone().wake();
        drop(st.waker.lock().unwrap().take());
        th.join().unwrap();
        drop(local);
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
        assert_freed(&rt);
    });
}

#[test]
fn local_cancellation_with_remote_wakers() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let local = crate::LocalWaker::from(st.waker());
        let remote = local.clone().into_waker();
        let th = thread::spawn(move || {
            let cloned = remote.clone();
            remote.wake_by_ref();
            cloned.wake();
            drop(remote);
        });
        local.clone().wake();
        task.cancel();
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        local.wake_by_ref();
        local.clone().wake();
        drop(st.waker.lock().unwrap().take());
        drop(local);
        th.join().unwrap();
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
        assert_freed(&rt);
    });
}

#[test]
fn local_waker_remote_shutdown() {
    model(|| {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let (st, task) = pending(&rt);
        let local = crate::LocalWaker::from(st.waker.lock().unwrap().take().unwrap());
        let remote = local.clone().into_waker();
        let th = thread::spawn(move || {
            let cloned = remote.clone();
            remote.wake_by_ref();
            cloned.wake();
            remote
        });
        local.clone().wake();
        drop(rt);
        assert!(task.is_finished());
        drop(task);
        local.wake_by_ref();
        local.clone().wake();
        let remote = th.join().unwrap();
        let converted = local.clone().into_waker();
        let th = thread::spawn(move || {
            let cloned = remote.clone();
            drop(remote);
            cloned.wake();
            converted.wake();
        });
        drop(local.clone());
        drop(local);
        th.join().unwrap();
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
        assert!(queue.upgrade().is_none(), "task alive after shutdown");
    });
}

/// A coalesced remote wake publishes the writes before it, the owner thread
/// may have consumed the pending wake before the readiness is set.
#[test]
fn coalesced_remote_wake() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let (st2, w) = (st.clone(), st.waker());
        let th = thread::spawn(move || {
            w.wake_by_ref();
            st2.ready.store(true, Ordering::Release);
            w.wake_by_ref();
            w
        });
        drain(&rt);
        let w = th.join().unwrap();
        drain(&rt);
        let finished = task.is_finished();
        drop(w);
        drop(st.waker.lock().unwrap().take());
        task.cancel();
        assert_freed(&rt);
        assert!(finished, "coalesced wake did not publish readiness");
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

/// A local join reads a result published after remote task submission.
#[test]
fn local_join_with_remote_submission() {
    model(|| {
        let rt = new_rt();
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let task = spawn(&rt, Fut(st.clone()));
        let handle = rt.handle();
        let th = thread::spawn(move || handle.spawn(async {}));
        drain(&rt);
        assert_eq!(*join(task).unwrap(), 42);
        th.join().unwrap();
        assert_freed(&rt);
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
    });
}

/// Detaching locally drops the awaiter while a remote wake completes the task.
#[test]
fn local_detached_awaiter_remote_wake() {
    model(|| {
        let rt = new_rt();
        let (st, mut task) = pending(&rt);
        let (st2, w) = (st.clone(), st.waker());
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(flag.clone());
        assert!(task.poll_join(&Context::from_waker(&waker)).is_pending());
        let th = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake();
        });
        drop(task);
        drop(waker);
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        th.join().unwrap();
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
        assert!(!flag.0.load(Ordering::Relaxed));
        assert_eq!(Arc::strong_count(&flag), 1);
    });
}

/// A remote wake completes a task with a local join.
#[test]
fn local_join_and_remote_wake() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let (st2, w) = (st.clone(), st.waker());
        let waker = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake();
        });
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        waker.join().unwrap();
        assert_eq!(*join(task).unwrap(), 42);
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
    });
}

/// Local cancellation races a remote wake.
#[test]
fn local_cancel_remote_wake() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let w = st.waker();
        let th = thread::spawn(move || {
            w.wake_by_ref();
            drop(w);
        });
        task.cancel();
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

/// A remote `wake_by_ref` races completion and runtime destruction. Retaining
/// the standard waker must not retain the runtime's driver.
#[test]
fn remote_wake_by_ref_shutdown() {
    model(|| {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let (st, task) = pending(&rt);
        drop(task);
        let w = st.waker.lock().unwrap().take().unwrap();
        let st2 = st.clone();
        let th = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake_by_ref();
            w
        });
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        drop(rt);
        drop(th.join().unwrap());
        assert!(queue.upgrade().is_none(), "task alive");
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

/// A task spawned from another thread discards its output on the owner.
#[test]
fn remote_spawn() {
    model(|| {
        let rt = new_rt();
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let (handle, fut) = (rt.handle(), Fut(st.clone()));
        let th = thread::spawn(move || {
            handle.spawn(fut);
        });
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        th.join().unwrap();
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

/// A local detach races remote wake publication; the output is dropped once.
#[test]
fn local_detach_remote_wake() {
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
        let waker = st.waker();
        let th = thread::spawn(move || waker.wake());
        drop(task);
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
            task::spawn_detached::<_, false>(
                async move {
                    let _ = fut.await;
                    Out(st2)
                },
                &q,
                Location::caller(),
            );
        });
        run_until(&rt, || st.outputs.load(Ordering::Relaxed) == 1);
        th.join().unwrap();
        assert_freed(&rt);
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
    });
}

/// A remote wake must resume the driver, including between poll and park.
#[test]
fn parked_remote_wake() {
    model(|| {
        let park = ParkNotify::default();
        let rt = Runtime::new(Box::new(park.clone()));
        let st = Arc::new(State::default());
        rt.awake();
        let task = spawn(&rt, Fut(st.clone()));
        assert_eq!(rt.poll(), PollResult::Pending);
        assert_eq!(st.polls.load(Ordering::Relaxed), 1);
        let (st2, w) = (st.clone(), st.waker());
        let th = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake();
        });
        run_parked(&rt, &park, || task.is_finished());
        th.join().unwrap();
        assert_eq!(*join(task).unwrap(), 42);
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
        assert_eq!(st.drops.load(Ordering::Relaxed), 1);
    });
}

/// Submissions may coalesce or straddle the notification reset and drain.
#[test]
fn parked_remote_submissions() {
    model(|| {
        let park = ParkNotify::default();
        let rt = Runtime::new(Box::new(park.clone()));
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let (handle, st2) = (rt.handle(), st.clone());
        let th = thread::spawn(move || {
            handle.spawn(Fut(st2.clone()));
            handle.spawn_detached(Fut(st2));
        });
        run_parked(&rt, &park, || st.drops.load(Ordering::Relaxed) == 2);
        th.join().unwrap();
        assert_eq!(st.polls.load(Ordering::Relaxed), 2);
        assert_freed(&rt);
    });
}

/// Independent producers race the same notification flag.
#[test]
fn parked_two_producers() {
    model(|| {
        let park = ParkNotify::default();
        let rt = Runtime::new(Box::new(park.clone()));
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let (handle, fut) = (rt.handle(), Fut(st.clone()));
                thread::spawn(move || handle.spawn_detached(fut))
            })
            .collect();
        run_parked(&rt, &park, || st.drops.load(Ordering::Relaxed) == 2);
        for th in threads {
            th.join().unwrap();
        }
        assert_eq!(st.polls.load(Ordering::Relaxed), 2);
        assert_freed(&rt);
    });
}

/// A failed notification must not strand subsequent coalesced submissions.
#[test]
fn parked_failed_notification() {
    #[derive(Debug)]
    struct FailOnceNotify {
        park: ParkNotify,
        failed: AtomicBool,
    }

    impl Notify for FailOnceNotify {
        fn notify(&self) -> io::Result<()> {
            if self.failed.swap(true, Ordering::Relaxed) {
                self.park.notify()
            } else {
                Err(io::ErrorKind::Interrupted.into())
            }
        }
    }

    for (producers, submissions) in [(1, 1), (1, 2), (2, 1)] {
        model(move || {
            let park = ParkNotify::default();
            let rt = Runtime::new(Box::new(FailOnceNotify {
                park: park.clone(),
                failed: AtomicBool::new(false),
            }));
            let st = Arc::new(State::default());
            st.ready.store(true, Ordering::Relaxed);
            let threads: Vec<_> = (0..producers)
                .map(|_| {
                    let (handle, st) = (rt.handle(), st.clone());
                    thread::spawn(move || {
                        for _ in 0..submissions {
                            handle.spawn_detached(Fut(st.clone()));
                        }
                    })
                })
                .collect();
            run_parked(&rt, &park, || {
                st.drops.load(Ordering::Relaxed) == producers * submissions
            });
            for th in threads {
                th.join().unwrap();
            }
            assert_eq!(st.polls.load(Ordering::Relaxed), producers * submissions);
            assert_freed(&rt);
        });
    }
}

/// The final remote release must not publish after shutdown's last drain.
#[test]
fn remote_release_shutdown() {
    model(|| {
        let rt = new_rt();
        let queue = Arc::downgrade(&rt.queue);
        let (st, task) = pending(&rt);
        drop(task);
        let w = st.waker.lock().unwrap().take().unwrap();
        let st2 = st.clone();
        let th = thread::spawn(move || {
            st2.ready.store(true, Ordering::Release);
            w.wake_by_ref();
            drop(w);
        });
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        drop(rt);
        th.join().unwrap();
        assert!(
            queue.upgrade().is_none(),
            "task alive after final remote release"
        );
    });
}

fn spawn_shutdown(detached: bool) {
    for producers in 1..=2 {
        model(move || {
            let rt = new_rt();
            let queue = Arc::downgrade(&rt.queue);
            let st = Arc::new(State::default());
            let threads: Vec<_> = (0..producers)
                .map(|_| {
                    let (handle, fut) = (rt.handle(), Fut(st.clone()));
                    thread::spawn(move || {
                        if detached {
                            handle.spawn_detached(fut);
                        } else {
                            handle.spawn(fut);
                        }
                    })
                })
                .collect();
            drop(rt);
            for th in threads {
                th.join().unwrap();
            }
            assert_eq!(
                st.drops.load(Ordering::Relaxed),
                producers,
                "late spawn retained its future"
            );
            assert!(
                queue.upgrade().is_none(),
                "task alive after concurrent spawn"
            );
        });
    }
}

/// Fire-and-forget spawning racing shutdown reclaims its future and queue.
#[test]
fn remote_spawn_shutdown() {
    spawn_shutdown(false);
}

/// Detached spawning has no join handle to participate in cleanup.
#[test]
fn remote_spawn_detached_shutdown() {
    spawn_shutdown(true);
}

/// A notifier can wait for shutdown without holding a publication permit.
#[test]
fn shutdown_with_blocked_notification() {
    #[derive(Debug)]
    struct BlockingNotify {
        entered: loom::sync::mpsc::Sender<()>,
        release: loom::sync::Mutex<loom::sync::mpsc::Receiver<()>>,
        interrupted: AtomicBool,
    }

    impl Notify for BlockingNotify {
        fn notify(&self) -> io::Result<()> {
            if !self.interrupted.swap(true, Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Ok(())
        }
    }

    for path in 0..3 {
        model(move || {
            let (entered, rx) = loom::sync::mpsc::channel();
            let (tx, release) = loom::sync::mpsc::channel();
            let rt = Runtime::new(Box::new(BlockingNotify {
                entered,
                release: loom::sync::Mutex::new(release),
                interrupted: AtomicBool::new(false),
            }));
            rt.awake();
            let queue = Arc::downgrade(&rt.queue);
            let st = Arc::new(State::default());
            let th = if path != 0 {
                let (handle, fut) = (rt.handle(), Fut(st.clone()));
                thread::spawn(move || {
                    if path == 1 {
                        handle.spawn(fut);
                    } else {
                        handle.spawn_detached(fut);
                    }
                })
            } else {
                let task = spawn(&rt, Fut(st.clone()));
                drain(&rt);
                let w = st.waker.lock().unwrap().take().unwrap();
                drop(task);
                thread::spawn(move || w.wake())
            };
            rx.recv().unwrap();
            drop(rt);
            assert_eq!(st.drops.load(Ordering::Relaxed), 1);
            assert!(queue.upgrade().is_some());
            tx.send(()).unwrap();
            th.join().unwrap();
            assert!(queue.upgrade().is_none(), "task alive");
        });
    }
}

/// Wake, release and join cancellation racing shutdown keep destruction local.
#[test]
fn remote_signal_shutdown() {
    struct OwnerDrop {
        owner: super::ThreadId,
        _local: std::rc::Rc<()>,
    }

    impl Drop for OwnerDrop {
        fn drop(&mut self) {
            assert_eq!(super::current_thread_id(), self.owner);
        }
    }

    for action in 0..4 {
        model(move || {
            let rt = new_rt();
            let queue = Arc::downgrade(&rt.queue);
            let st = Arc::new(State::default());
            let fut = Fut(st.clone());
            let owner = OwnerDrop {
                owner: super::current_thread_id(),
                _local: std::rc::Rc::new(()),
            };
            let task = spawn(&rt, async move {
                let _owner = owner;
                fut.await
            });
            drain(&rt);
            let w = st.waker.lock().unwrap().take().unwrap();
            let th = thread::spawn(move || match action {
                0 => drop(w),
                1 => w.wake(),
                _ => w.wake_by_ref(),
            });
            if action == 3 {
                task.cancel();
            } else {
                drop(task);
            }
            drop(rt);
            th.join().unwrap();
            assert_eq!(st.drops.load(Ordering::Relaxed), 1);
            drop(st.waker.lock().unwrap().take());
            assert!(queue.upgrade().is_none(), "task alive");
        });
    }
}
