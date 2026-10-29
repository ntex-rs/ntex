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

/// STATEMENT: A remote wake is not lost while the owner thread services a pending task.
///
/// HOW:
/// - Poll once, then set readiness and consume a cloned waker on a modeled producer thread.
/// - Run the owner until completion, join the result as 42, and release the saved waker.
/// - Check one future drop, empty queues, and zero task allocations.
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

/// STATEMENT: A remote consuming wake survives release of the final local interest.
///
/// HOW:
/// - Detach a pending task while retaining its standard waker and a local wrapper.
/// - Race remote readiness plus consuming wake against dropping the local wrapper.
/// - Run until destruction and check exactly two polls and full task reclamation.
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

/// STATEMENT: Remote clone acquisition remains ordered when source and clone return to the owner.
///
/// HOW:
/// - Clone a pending task's waker on a producer while the owner drops its task handle and drains.
/// - Return both wakers, drop them on the owner, and service their remaining FIFO commands.
/// - Check exactly one future drop and no retained task allocations or queue entries.
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

/// STATEMENT: First remote clone or wake publication safely races owner release of a completed task.
///
/// HOW:
/// - Complete a task returning its waker, then remotely clone, wake by reference, or consume that waker.
/// - Drop the completed handle and drain concurrently with each producer operation.
/// - Join the producer, release returned references, and check empty queues and zero allocations.
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

/// STATEMENT: Owner release cannot overtake remote releases followed by acquisition of another clone.
///
/// HOW:
/// - Prime the owner's endpoint cache so idle releases use its open queue fast path.
/// - Detach a pending task, then remotely drop two wakers and clone the surviving source.
/// - Return source and clone; drop the source on the owner and drain, checking the future stays live.
/// - Drop the last clone and check one future destruction and complete reclamation.
#[test]
fn owner_release_follows_pending_remote_releases() {
    model(|| {
        let rt = new_rt();
        drop(super::wake::remote_queue(rt.queue.runtime_id).unwrap());
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

/// STATEMENT: A remote clone racing endpoint unregister leaves inert late clones and no retained driver.
///
/// HOW:
/// - Race remote clone plus consuming wake against dropping the pending task's runtime.
/// - Check cloning the returned late waker no longer uses the native vtable and exercise its wake.
/// - Drop the task handle and check one future destruction and an expired queue weak reference.
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

/// STATEMENT: A primed remote endpoint cache cannot keep native cloning or the driver alive after shutdown.
///
/// HOW:
/// - Prime a producer's endpoint cache with clone/drop and signal the owner before another clone and wake.
/// - Shut down concurrently, then check a clone of the returned waker uses a non-native vtable.
/// - Consume that waker, drop the task, and check one future drop and queue expiration.
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

/// STATEMENT: Owned remote queue lookup can race shutdown without preserving admission or leaking the queue.
///
/// HOW:
/// - Optionally prime the remote cache, then race another lookup against runtime shutdown.
/// - Wait for shutdown and check admission on any retained queue is rejected and lookup returns None.
/// - Release the producer and check the queue allocation expires.
#[test]
fn remote_queue_lookup_races_shutdown() {
    for cached in [false, true] {
        model(move || {
            let rt = new_rt();
            let id = rt.queue.runtime_id;
            let queue = Arc::downgrade(&rt.queue);
            let (primed, ready) = loom::sync::mpsc::channel();
            let (closed, shutdown) = loom::sync::mpsc::channel();
            let producer = thread::spawn(move || {
                if cached {
                    drop(super::wake::remote_queue(id).unwrap());
                }
                primed.send(()).unwrap();
                let retained = super::wake::remote_queue(id);
                shutdown.recv().unwrap();
                if let Some(queue) = retained {
                    assert!(queue.admit(false).is_none());
                }
                assert!(super::wake::remote_queue(id).is_none());
            });
            ready.recv().unwrap();
            drop(rt);
            closed.send(()).unwrap();
            producer.join().unwrap();
            assert!(queue.upgrade().is_none());
        });
    }
}

/// STATEMENT: Pruning a multi-runtime remote cache can race shutdown without losing live endpoints.
///
/// HOW:
/// - Prime two remote endpoints, then race shutdown of the first against a miss for a third endpoint.
/// - Wake the first and second again, exercising both rejected and admitted cached publication.
/// - Keep completed task handles alive for the borrowed wakers, then check queue and task reclamation.
#[test]
fn cached_remote_endpoints_prune_during_shutdown() {
    model(|| {
        let first = new_rt();
        let second = new_rt();
        let third = new_rt();
        let mut tasks = [&first, &second, &third].map(|rt| {
            spawn(
                rt,
                std::future::poll_fn(|cx| {
                    Poll::Ready(std::mem::ManuallyDrop::new(unsafe {
                        Waker::from_raw(super::wake::raw_waker(cx.waker().data()))
                    }))
                }),
            )
        });
        for rt in [&first, &second, &third] {
            drain(rt);
        }
        let wakers = tasks.each_mut().map(|task| {
            let Poll::Ready(Some(waker)) = task.poll_join(&Context::from_waker(Waker::noop()))
            else {
                panic!("missing borrowed waker");
            };
            waker
        });
        let queue = Arc::downgrade(&first.queue);
        let allocations = Arc::downgrade(&first.queue.task_allocations);
        let (primed, ready) = loom::sync::mpsc::channel();
        let producer = thread::spawn(move || {
            wakers[0].wake_by_ref();
            wakers[1].wake_by_ref();
            primed.send(()).unwrap();
            wakers[2].wake_by_ref();
            wakers[0].wake_by_ref();
            wakers[1].wake_by_ref();
        });
        ready.recv().unwrap();
        drop(first);
        producer.join().unwrap();
        drop(tasks);
        assert!(queue.upgrade().is_none());
        assert!(allocations.upgrade().is_none());
        assert_freed(&second);
        assert_freed(&third);
    });
}

/// STATEMENT: Shutting down one cached owner endpoint does not invalidate another live runtime's wakers.
///
/// HOW:
/// - Prime local routes for two runtimes and remotely clone/drop a waker belonging to the second.
/// - Drain both, destroy the first, and compare converted local-waker vtables with the native vtable.
/// - Check only the first becomes inert until the second shuts down, then release all handles.
/// - Check each pending future is destroyed exactly once.
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

/// STATEMENT: A coalesced wake publishes writes made after an earlier wake was already queued.
///
/// HOW:
/// - Queue an initial remote wake for a pending task without draining it.
/// - Write 42 through a Loom-tracked cell, wake again, and signal only through a relaxed flag.
/// - Drain and check the joined task reads 42 before releasing the producer and checking reclamation.
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

/// STATEMENT: Local completion safely overlaps remote standard-waker and fallback-wrapper operations.
///
/// HOW:
/// - Save a local wrapper and send a converted clone to a producer that clones, wraps, wakes, and drops it.
/// - Set readiness, wake locally, drain, and join the result as 42 while remote operations race.
/// - Exercise a stale local wake, release all wakers, and check one future drop and full reclamation.
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

/// STATEMENT: Local cancellation wins safely over concurrent local and remote waker operations.
///
/// HOW:
/// - Retain a local wrapper while a producer clones, wakes, and releases a converted standard waker.
/// - Wake locally, cancel the task, and run until its future drops.
/// - Exercise stale local wakes, release every waker, and check one drop with no retained allocations.
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

/// STATEMENT: Local and converted remote wakers remain safe across shutdown without retaining the queue.
///
/// HOW:
/// - Race remote clone/wake operations with a local wake and runtime destruction.
/// - Check the local handle finishes, then exercise stale local wakes and convert another wrapper.
/// - Release local handles while workers wake the remote survivors; check one future drop and queue expiration.
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

/// STATEMENT: A second remote wake publishes readiness even if the owner consumed the first wake.
///
/// HOW:
/// - Have a producer wake, publish readiness, and wake again while the owner drains concurrently.
/// - Join the producer, drain again, and save the task's finished state before cancellation cleanup.
/// - Check completion was observed and all references and queue entries are reclaimed.
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

/// STATEMENT: Two independent wake producers can complete separate tasks through the shared remote queue.
///
/// HOW:
/// - Poll two tasks pending, then set readiness and consume each waker on a separate producer thread.
/// - Run until both handles finish and join both producers.
/// - Release handles and saved wakers; check one drop per future and no retained allocations.
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

/// STATEMENT: A local join and concurrent remote submission both progress without retaining task storage.
///
/// HOW:
/// - Spawn a ready local future while a producer submits an unrelated ready task through a runtime handle.
/// - Drain and join the local result as 42, then join the producer and drain any remaining submission.
/// - Check one local future drop, empty queues, and zero task allocations.
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

/// STATEMENT: Local detachment releases a registered join awaiter without waking it during remote completion.
///
/// HOW:
/// - Register a flag waker on a pending join while a producer publishes readiness and consumes a task waker.
/// - Drop the task handle and external awaiter waker, then run until the future drops.
/// - Join the producer and clean up; check the awaiter flag stays false and only its test reference remains.
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

/// STATEMENT: A locally retained join receives the result of a task completed by a remote wake.
///
/// HOW:
/// - Poll pending, then publish readiness and consume a cloned waker on a producer thread.
/// - Run until the future drops, join the producer, and check the local join result is 42.
/// - Release the saved waker and check empty queues and zero task allocations.
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

/// STATEMENT: Local cancellation racing a remote borrowed wake destroys the future without excess polling.
///
/// HOW:
/// - Wake and release a cloned standard waker remotely while cancelling its pending task locally.
/// - Exercise both explicit cancellation and cancellation-on-drop.
/// - Run until destruction, join the producer, and release the saved waker.
/// - Check full reclamation and at most two total future polls.
#[test]
fn local_cancel_remote_wake() {
    for cancel_on_drop in [false, true] {
        model(move || {
            let rt = new_rt();
            let (st, task) = pending(&rt);
            task.set_cancel_on_drop(cancel_on_drop);
            let w = st.waker();
            let th = thread::spawn(move || {
                w.wake_by_ref();
                drop(w);
            });
            if cancel_on_drop {
                drop(task);
            } else {
                task.cancel();
            }
            run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
            th.join().unwrap();
            drop(st.waker.lock().unwrap().take());
            assert_freed(&rt);
            assert!(st.polls.load(Ordering::Relaxed) <= 2);
        });
    }
}

/// STATEMENT: Concurrent owner-handle and remote-waker release reclaim a detached pending task.
///
/// HOW:
/// - Move the saved waker to a producer that publishes readiness, wakes by reference, and drops it.
/// - Drop the task handle concurrently and service the owner until the future drops.
/// - Join the producer, release any saved waker, and check no queue entries or allocations remain.
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

/// STATEMENT: Completion and shutdown tolerate a remote borrowed wake and late standard-waker release.
///
/// HOW:
/// - Detach a pending task and remotely publish readiness and wake by reference while retaining the waker.
/// - Run until its future drops, then destroy the runtime before joining the producer and releasing that waker.
/// - Check the queue weak reference expires rather than retaining the runtime driver.
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

/// STATEMENT: Post-shutdown output release safely races remote clone/drop of its surviving task waker.
///
/// HOW:
/// - Complete a task with a counted output, retain its standard waker, and destroy the runtime.
/// - Drop the local handle while a producer clones and drops that waker.
/// - Check one future drop, one output drop, and expiration of the queue weak reference.
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

/// STATEMENT: A remote fire-and-forget submission discards its result and releases owner task storage.
///
/// HOW:
/// - Submit a ready future through a producer's runtime handle without retaining its join handle.
/// - Run the owner until the future drops, then join the producer.
/// - Drain and check empty queues and zero task allocations.
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

/// STATEMENT: Local detachment racing remote wake publication drops the completed output exactly once.
///
/// HOW:
/// - Poll a counted-output task pending, publish readiness, and consume its waker on a producer.
/// - Drop the local handle while draining the owner, then join the producer and release the saved waker.
/// - Drain remaining work, check full reclamation, and require exactly one output destruction.
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

/// STATEMENT: Remote detached publication tolerates immediate owner completion and reclamation.
///
/// HOW:
/// - Publish a ready future with a counted output through the low-level detached spawn path on a producer.
/// - Run the owner until the output drops while publication may still be returning.
/// - Join the producer and check one future drop, empty queues, and zero task allocations.
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

/// STATEMENT: A remote wake resumes a parked driver even when notification falls between poll and wait.
///
/// HOW:
/// - Mark the runtime awake, poll a task pending once, then remotely publish readiness and consume its waker.
/// - Service it with the notification-dependent parked driver rather than a spinning drain loop.
/// - Join the result as 42, release saved references, and check one future drop and complete reclamation.
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

/// STATEMENT: Coalesced or reset-straddling remote submissions both wake a parked driver.
///
/// HOW:
/// - Submit one ordinary and one detached ready future from the same modeled producer.
/// - Use the parked driver until both futures drop, then join the producer.
/// - Check exactly two polls and no retained task allocations or queue entries.
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

/// STATEMENT: Independent producers sharing a notification flag cannot strand a parked driver's tasks.
///
/// HOW:
/// - Submit one detached ready future from each of two modeled producer threads.
/// - Run the parked driver until both futures drop and join both producers.
/// - Check exactly two polls, empty queues, and zero task allocations.
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

/// STATEMENT: An interrupted notification is retried without stranding later coalesced submissions.
///
/// HOW:
/// - Make the first notification return `Interrupted`, then forward later attempts to the parked notifier.
/// - Model one or two producers with one or two submissions and run the notification-dependent driver.
/// - Check every submitted future is polled and dropped once and all task storage is reclaimed.
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

/// STATEMENT: Final remote wake/reference release cannot leave a queue retained past shutdown's last drain.
///
/// HOW:
/// - Detach a pending task while a producer publishes readiness, wakes by reference, and drops its last waker.
/// - Run until the future drops, destroy the runtime, and join the producer.
/// - Check the queue weak reference has expired after that final remote release.
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

/// STATEMENT: Fire-and-forget remote spawning racing shutdown reclaims every future and its queue.
///
/// HOW:
/// - Use `spawn_shutdown(false)` to race one or two ordinary remote submissions against runtime drop.
/// - Join all producers and check one future destruction per submission and an expired queue weak reference.
#[test]
fn remote_spawn_shutdown() {
    spawn_shutdown(false);
}

/// STATEMENT: Remote detached spawning racing shutdown reclaims futures without join-handle cleanup.
///
/// HOW:
/// - Use `spawn_shutdown(true)` to race one or two detached submissions against runtime drop.
/// - Join every producer and check each future drops and the queue weak reference expires.
#[test]
fn remote_spawn_detached_shutdown() {
    spawn_shutdown(true);
}

/// STATEMENT: A blocked notifier does not hold a publication permit that prevents shutdown.
///
/// HOW:
/// - Model remote wake, ordinary spawn, and detached spawn with an interrupted then blocking notifier.
/// - Wait for notifier entry, destroy the runtime, and check one future drop while the notifier retains the queue.
/// - Release and join the notifier thread, then check the queue weak reference expires.
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

/// STATEMENT: Remote wake/release and local handle cancellation racing shutdown keep future destruction local.
///
/// HOW:
/// - Poll a future carrying an owner-only drop guard and move its saved waker to a modeled producer.
/// - Vary remote drop, consuming wake, or borrowed wake while locally dropping or cancelling the handle.
/// - Destroy the runtime, join the producer, and check one owner-thread future drop and queue expiration.
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
