//! Loom models of the task protocol between the owner thread and other
//! threads: the remote word, the remote queue, the join handle awaiter and
//! the output hand-off, scheduler shutdown and parked-driver notifications.
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
    unsafe { task::spawn::<_, true>(f, &rt.queue, Location::caller()) }
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
fn local_waker_remote_cancellation() {
    model(|| {
        let rt = new_rt();
        let (st, task) = pending(&rt);
        let local = crate::LocalWaker::from(st.waker());
        let remote = local.clone().into_waker();
        let th = thread::spawn(move || {
            let cloned = remote.clone();
            remote.wake_by_ref();
            task.cancel();
            cloned.wake();
            drop(remote);
        });
        local.clone().wake();
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

/// The join handle on another thread registers and is dropped before a
/// remote wake completes the task, the awaiter is dropped without a wake.
#[test]
fn remote_detached_awaiter() {
    model(|| {
        let rt = new_rt();
        let (st, mut task) = pending(&rt);
        let (st2, w) = (st.clone(), st.waker());
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(flag.clone());
        let th = thread::spawn(move || {
            assert!(task.poll_join(&Context::from_waker(&waker)).is_pending());
            drop(task);
            st2.ready.store(true, Ordering::Release);
            w.wake();
        });
        run_until(&rt, || st.drops.load(Ordering::Relaxed) == 1);
        th.join().unwrap();
        drop(st.waker.lock().unwrap().take());
        assert_freed(&rt);
        assert!(!flag.0.load(Ordering::Relaxed));
        assert_eq!(Arc::strong_count(&flag), 1);
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

/// A remote `wake_by_ref` queues the task without a queue reference while
/// the owner thread completes the task and drops the runtime, the waker
/// keeps the queue alive.
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

/// A task spawned from another thread is run and joined there.
#[test]
fn remote_spawn() {
    model(|| {
        let rt = new_rt();
        let st = Arc::new(State::default());
        st.ready.store(true, Ordering::Relaxed);
        let (q, fut) = (rt.queue.clone(), Fut(st.clone()));
        let th = thread::spawn(move || {
            let task = unsafe { task::spawn::<_, false>(fut, &q, Location::caller()) };
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
            handle.spawn(Fut(st2.clone())).detach();
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
                    let (q, fut) = (rt.queue.clone(), Fut(st.clone()));
                    thread::spawn(move || unsafe {
                        if detached {
                            task::spawn_detached::<_, false>(fut, &q, Location::caller());
                        } else {
                            drop(task::spawn::<_, false>(fut, &q, Location::caller()));
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

/// Joined spawning racing shutdown must reclaim its future and queue.
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
                        drop(handle.spawn(fut));
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
            let th = thread::spawn(move || {
                match action {
                    0 => drop(w),
                    1 => w.wake(),
                    _ => w.wake_by_ref(),
                }
                if action == 3 {
                    task.cancel();
                } else {
                    drop(task);
                }
            });
            drop(rt);
            th.join().unwrap();
            assert_eq!(st.drops.load(Ordering::Relaxed), 1);
            drop(st.waker.lock().unwrap().take());
            assert!(queue.upgrade().is_none(), "task alive");
        });
    }
}
