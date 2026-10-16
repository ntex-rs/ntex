use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{future::Future, io, thread};

use async_task::Runnable;
use crossbeam_queue::SegQueue;
use swap_buffer_queue::error::{TryDequeueError, TryEnqueueError};
use swap_buffer_queue::{Queue, buffer::ArrayBuffer};

use crate::{
    busy::BusyClock, driver::Driver, driver::Notify, driver::PollResult, handle::JoinHandle,
};

scoped_tls::scoped_thread_local!(static CURRENT_RUNTIME: Runtime);

thread_local!(static THREAD_ID: thread::ThreadId = thread::current().id());

/// Returns the id of the current thread without cloning its `Thread` handle.
fn current_thread_id() -> thread::ThreadId {
    THREAD_ID.with(|id| *id)
}

#[derive(Debug)]
/// The async runtime for ntex.
///
/// This is a thread-local runtime and cannot be sent to other threads.
pub struct Runtime {
    stop: Cell<bool>,
    queue: Arc<RunnableQueue>,
}

impl Runtime {
    /// Creates a runtime with default configuration.
    pub fn new(handle: Box<dyn Notify>) -> Self {
        Self::builder().build(handle)
    }

    /// Creates a runtime builder.
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::new()
    }

    #[allow(clippy::arc_with_non_send_sync)]
    fn with_builder(builder: &RuntimeBuilder, handle: Box<dyn Notify>) -> Self {
        Self {
            stop: Cell::new(false),
            queue: Arc::new(RunnableQueue::new(builder.event_interval, handle)),
        }
    }

    /// Runs a closure with the runtime active on the current thread.
    ///
    /// ## Panics
    ///
    /// Panics if no runtime is active on the current thread.
    pub fn with_current<T, F: FnOnce(&Self) -> T>(f: F) -> T {
        #[cold]
        fn not_in_neon_runtime() -> ! {
            panic!("not in a neon runtime")
        }

        if CURRENT_RUNTIME.is_set() {
            CURRENT_RUNTIME.with(f)
        } else {
            not_in_neon_runtime()
        }
    }

    /// Returns [`Runtime::now`] of the runtime active on the current thread,
    /// or [`Instant::now()`] if no runtime is active.
    pub(crate) fn current_now() -> Instant {
        if CURRENT_RUNTIME.is_set() {
            CURRENT_RUNTIME.with(Runtime::now)
        } else {
            Instant::now()
        }
    }

    #[inline]
    /// Returns a handle to this runtime.
    pub fn handle(&self) -> Handle {
        Handle {
            queue: self.queue.clone(),
        }
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        unsafe { self.spawn_unchecked(future) }
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// # Safety
    ///
    /// The caller should ensure the captured lifetime is long enough.
    pub unsafe fn spawn_unchecked<F: Future>(&self, future: F) -> JoinHandle<F::Output> {
        let queue = self.queue.clone();
        let (runnable, task) = unsafe {
            async_task::spawn_unchecked(future, move |runnable| {
                queue.schedule(runnable);
            })
        };
        runnable.schedule();
        JoinHandle::new(task)
    }

    /// Marks the runtime as awake.
    ///
    /// Drivers call this after returning from a blocking wait, before handling
    /// events that wake tasks. Tasks scheduled from the runtime thread do not
    /// notify the driver until [`poll`](Self::poll) finds no more tasks to run,
    /// the driver must call `poll` before blocking again.
    ///
    /// Starts a busy period, see [`busy_duration`](Self::busy_duration).
    #[inline]
    pub fn awake(&self) {
        if self.queue.idle.get() {
            self.queue.start_busy();
        }
    }

    /// Polls the runtime and runs scheduled tasks.
    pub fn poll(&self) -> PollResult {
        // tasks scheduled from other threads do not end the idle state
        if self.queue.idle.get() {
            self.queue.busy.start(Instant::now());
        }

        let result = if self.stop.get() {
            PollResult::Ready
        } else if self.queue.run() {
            PollResult::PollAgain
        } else {
            PollResult::Pending
        };

        // a runtime that never idles stays visible, its busy period is read
        // from its start
        let now = Instant::now();
        if result == PollResult::PollAgain {
            self.queue.poll_start.set(now);
            self.queue.busy.next_turn(now);
        } else {
            self.queue.idle.set(true);
            self.queue.busy.stop(now);
        }
        result
    }

    /// Returns the start of the current runtime turn.
    ///
    /// While the runtime is busy, returns the time the driver woke up or the
    /// previous [`poll`](Self::poll) finished, tasks polled in the same turn
    /// observe the same instant. Returns [`Instant::now()`] while the runtime
    /// is idle.
    #[inline]
    pub fn now(&self) -> Instant {
        if self.queue.idle.get() {
            Instant::now()
        } else {
            self.queue.poll_start.get()
        }
    }

    /// Returns the total time the runtime has been busy.
    ///
    /// The runtime is busy from [`awake`](Self::awake) until [`poll`](Self::poll)
    /// finds no more tasks to run. This includes running tasks and handling
    /// driver events, and excludes time the driver is blocked waiting for events.
    /// The value is wall-clock time, not CPU time, and includes the current
    /// busy period, so it can be read from other threads while a task runs.
    ///
    /// Utilization over an interval is the busy duration delta divided by the
    /// elapsed time of the interval.
    pub fn busy_duration(&self) -> Duration {
        self.queue.busy.busy_duration()
    }

    /// Returns how long the current runtime turn has been running, `None`
    /// while the runtime is idle.
    ///
    /// A turn ends when [`poll`](Self::poll) returns, a long turn means a task
    /// or a driver event handler keeps the runtime from making progress.
    pub fn current_turn(&self) -> Option<Duration> {
        self.queue.busy.current_turn()
    }

    /// Runs the provided future.
    ///
    /// Blocks the current thread until the future completes.
    ///
    /// # Panics
    ///
    /// Panics if the driver fails to run the provided future.
    pub fn block_on<F: Future>(&self, future: F, driver: &dyn Driver) -> F::Output {
        self.stop.set(false);
        self.queue.poll_start.set(Instant::now());

        CURRENT_RUNTIME.set(self, || {
            let mut result = None;
            unsafe {
                self.spawn_unchecked(async {
                    result = Some(future.await);
                    self.stop.set(true);
                    let _ = self.queue.handle.notify();
                });
            }

            ntex_error::set_backtrace_start_alt("src/raw.rs", 0);
            driver.run(self).expect("Driver failed");
            result.expect("Driver failed to poll")
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        CURRENT_RUNTIME.set(self, || {
            self.queue.clear();
        });
    }
}

#[derive(Debug)]
/// A thread-safe handle used to schedule work on a runtime.
pub struct Handle {
    queue: Arc<RunnableQueue>,
}

impl Handle {
    /// Returns a handle to the runtime active on the current thread.
    ///
    /// # Panics
    ///
    /// Panics if no runtime is active on the current thread.
    pub fn current() -> Handle {
        Runtime::with_current(Runtime::handle)
    }

    /// Wakes the runtime's driver.
    pub fn notify(&self) -> io::Result<()> {
        self.queue.handle.notify()
    }

    /// Returns the total time the runtime has been busy.
    ///
    /// See [`Runtime::busy_duration`]. Always returns `Some`, the return type
    /// matches the other runtime backends.
    pub fn busy_duration(&self) -> Option<Duration> {
        Some(self.queue.busy.busy_duration())
    }

    /// Returns how long the current runtime turn has been running, `None`
    /// while the runtime is idle.
    ///
    /// See [`Runtime::current_turn`].
    pub fn current_turn(&self) -> Option<Duration> {
        self.queue.busy.current_turn()
    }

    /// Spawns a new asynchronous task, returning a [`JoinHandle`] for it.
    ///
    /// Spawning a task enables the task to execute concurrently to other tasks.
    /// There is no guarantee that a spawned task will execute to completion.
    pub fn spawn<F: Future + Send + 'static>(&self, future: F) -> JoinHandle<F::Output> {
        let queue = self.queue.clone();
        let schedule = move |runnable| {
            queue.schedule(runnable);
        };
        let (runnable, task) = unsafe { async_task::spawn_unchecked(future, schedule) };
        runnable.schedule();
        JoinHandle::new(task)
    }
}

impl Clone for Handle {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
        }
    }
}

#[derive(Debug)]
struct RunnableQueue {
    id: thread::ThreadId,
    idle: Cell<bool>,
    // start of the current busy period, valid while not idle
    poll_start: Cell<Instant>,
    // set by the first remote `schedule` after `run` cleared it
    notified: AtomicBool,
    busy: BusyClock,
    handle: Box<dyn Notify>,
    event_interval: usize,
    local_queue: UnsafeCell<VecDeque<Runnable>>,
    sync_fixed_queue: Queue<ArrayBuffer<Runnable, 128>>,
    sync_queue: SegQueue<Runnable>,
}

unsafe impl Send for RunnableQueue {}
unsafe impl Sync for RunnableQueue {}

impl RunnableQueue {
    fn new(event_interval: usize, handle: Box<dyn Notify>) -> Self {
        Self {
            handle,
            event_interval,
            id: current_thread_id(),
            idle: Cell::new(true),
            poll_start: Cell::new(Instant::now()),
            notified: AtomicBool::new(false),
            busy: BusyClock::new(),
            local_queue: UnsafeCell::new(VecDeque::new()),
            sync_fixed_queue: Queue::default(),
            sync_queue: SegQueue::new(),
        }
    }

    fn schedule(&self, runnable: Runnable) {
        if self.id == current_thread_id() {
            unsafe { (*self.local_queue.get()).push_back(runnable) };
            if self.idle.get() {
                // starts a busy period, `awake` does not reset it
                self.start_busy();
                self.handle.notify().ok();
            }
        } else {
            let result = self.sync_fixed_queue.try_enqueue([runnable]);
            if let Err(TryEnqueueError::InsufficientCapacity([runnable])) = result {
                self.sync_queue.push(runnable);
            }
            // the driver is already notified, `run` drains this task after
            // clearing the flag
            if !self.notified.swap(true, Ordering::Release) {
                self.handle.notify().ok();
            }
        }
    }

    fn run(&self) -> bool {
        // cleared before draining the sync queues, a task enqueued after the
        // drain notifies the driver again
        if self.notified.load(Ordering::Relaxed) {
            self.notified.swap(false, Ordering::Acquire);
        }

        // a running task may schedule into `local_queue`, so it must not be
        // borrowed across `task.run()`
        for _ in 0..self.event_interval {
            if let Some(task) = self.pop_local() {
                task.run();
            } else {
                break;
            }
        }

        let sync_queue_fixed = match self.sync_fixed_queue.try_dequeue() {
            Ok(buf) => {
                for task in buf {
                    task.run();
                }
                false
            }
            Err(TryDequeueError::Empty | TryDequeueError::Closed) => false,
            Err(_) => true,
        };

        let sync_queue = {
            for _ in 0..self.event_interval {
                if let Some(task) = self.sync_queue.pop() {
                    task.run();
                } else {
                    break;
                }
            }
            !self.sync_queue.is_empty()
        };

        // tasks from other threads may schedule local tasks
        let local_queue = unsafe { !(*self.local_queue.get()).is_empty() };
        let more_tasks = local_queue || sync_queue_fixed || sync_queue;
        if !more_tasks {
            self.idle.set(true);
        }
        more_tasks
    }

    fn start_busy(&self) {
        let now = Instant::now();
        self.idle.set(false);
        self.poll_start.set(now);
        self.busy.start(now);
    }

    fn clear(&self) {
        while self.sync_queue.pop().is_some() {}
        while self.sync_fixed_queue.try_dequeue().is_ok() {}
        // dropped tasks may schedule other tasks, drop each outside of the borrow
        while let Some(task) = self.pop_local() {
            drop(task);
        }
    }

    fn pop_local(&self) -> Option<Runnable> {
        unsafe { (*self.local_queue.get()).pop_front() }
    }
}

/// Builder for [`Runtime`].
#[derive(Debug, Clone)]
pub struct RuntimeBuilder {
    event_interval: usize,
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl RuntimeBuilder {
    /// Create the builder with default config.
    pub fn new() -> Self {
        Self { event_interval: 61 }
    }

    /// Sets the number of scheduler ticks after which the scheduler will poll
    /// for external events (timers, I/O, and so on).
    ///
    /// A scheduler “tick” roughly corresponds to one poll invocation on a task.
    /// Values below 1 are treated as 1.
    pub fn event_interval(&mut self, val: usize) -> &mut Self {
        self.event_interval = val.max(1);
        self
    }

    /// Build [`Runtime`].
    pub fn build(&self, handle: Box<dyn Notify>) -> Runtime {
        Runtime::with_builder(self, handle)
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Poll, Waker};
    use std::{cell::RefCell, future::poll_fn, rc::Rc};

    use super::*;

    #[derive(Debug)]
    struct NoopNotify;

    impl Notify for NoopNotify {
        fn notify(&self) -> io::Result<()> {
            Ok(())
        }
    }

    struct WakeOnDrop(Rc<RefCell<Option<Waker>>>);

    impl Drop for WakeOnDrop {
        fn drop(&mut self) {
            if let Some(w) = self.0.borrow_mut().take() {
                w.wake();
            }
        }
    }

    #[derive(Debug, Default)]
    struct CountNotify(Arc<std::sync::atomic::AtomicUsize>);

    impl Notify for CountNotify {
        fn notify(&self) -> io::Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn awake_skips_local_notify() {
        use std::sync::atomic::Ordering;

        let cnt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rt = Runtime::new(Box::new(CountNotify(cnt.clone())));

        // the driver is awake and polls the runtime before blocking
        rt.awake();
        rt.spawn(async {}).detach();
        assert_eq!(cnt.load(Ordering::Relaxed), 0);
        assert_eq!(rt.poll(), PollResult::Pending);

        // the runtime is idle, the driver may block
        rt.spawn(async {}).detach();
        rt.spawn(async {}).detach();
        assert_eq!(cnt.load(Ordering::Relaxed), 1);
        assert_eq!(rt.poll(), PollResult::Pending);
    }

    #[test]
    fn remote_schedule_notifies_once() {
        let cnt = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rt = Runtime::new(Box::new(CountNotify(cnt.clone())));
        let spawn = |n| {
            let hnd = rt.handle();
            std::thread::spawn(move || {
                for _ in 0..n {
                    hnd.spawn(async {}).detach();
                }
            })
            .join()
            .unwrap();
        };

        spawn(100);
        assert_eq!(cnt.load(Ordering::Relaxed), 1);
        assert_eq!(rt.poll(), PollResult::Pending);

        // `poll` clears the flag
        spawn(100);
        assert_eq!(cnt.load(Ordering::Relaxed), 2);
        assert_eq!(rt.poll(), PollResult::Pending);
    }

    #[test]
    fn remote_schedule_no_lost_wakeup() {
        use std::sync::{Condvar, Mutex, atomic::AtomicUsize};
        use std::time::Duration;

        #[derive(Debug, Default)]
        struct Event(Mutex<bool>, Condvar);

        #[derive(Debug)]
        struct EventNotify(Arc<Event>);

        impl Notify for EventNotify {
            fn notify(&self) -> io::Result<()> {
                *self.0.0.lock().unwrap() = true;
                self.0.1.notify_one();
                Ok(())
            }
        }

        const THREADS: usize = 4;
        const TASKS: usize = 10_000;

        let ev = Arc::new(Event::default());
        let rt = Runtime::new(Box::new(EventNotify(ev.clone())));
        let cnt = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..THREADS)
            .map(|_| {
                let (hnd, cnt) = (rt.handle(), cnt.clone());
                std::thread::spawn(move || {
                    for _ in 0..TASKS {
                        let cnt = cnt.clone();
                        hnd.spawn(async move {
                            cnt.fetch_add(1, Ordering::Relaxed);
                        })
                        .detach();
                    }
                })
            })
            .collect();

        // blocks like a driver, a lost wakeup times out
        while cnt.load(Ordering::Relaxed) < THREADS * TASKS {
            if rt.poll() == PollResult::Pending {
                let guard = ev.0.lock().unwrap();
                let (mut guard, res) =
                    ev.1.wait_timeout_while(guard, Duration::from_secs(5), |n| !*n)
                        .unwrap();
                assert!(!res.timed_out(), "lost wakeup");
                *guard = false;
            }
        }
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn local_task_woken_by_remote_task() {
        use std::sync::{Mutex, atomic::AtomicBool, atomic::Ordering};

        let rt = Runtime::new(Box::new(NoopNotify));
        let waker = Arc::new(Mutex::new(None::<Waker>));
        let done = Rc::new(AtomicBool::new(false));
        let (waker2, done2) = (waker.clone(), done.clone());
        rt.spawn(poll_fn(move |cx| {
            if waker2.lock().unwrap().replace(cx.waker().clone()).is_some() {
                done2.store(true, Ordering::Relaxed);
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
        .detach();
        assert_eq!(rt.poll(), PollResult::Pending);

        let hnd = rt.handle();
        std::thread::spawn(move || {
            hnd.spawn(async move {
                waker.lock().unwrap().clone().unwrap().wake();
            })
            .detach();
        })
        .join()
        .unwrap();

        // the remote task schedules the local task, it must be polled again
        assert_eq!(rt.poll(), PollResult::PollAgain);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(done.load(Ordering::Relaxed));
    }

    #[test]
    fn schedule_while_running() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let done = Rc::new(RefCell::new(0));
        let done2 = done.clone();
        rt.spawn(async move {
            // schedules into the local queue from a running task
            let h = Runtime::with_current(|rt| rt.spawn(async { 1 }));
            *done2.borrow_mut() = h.await.unwrap();
        })
        .detach();
        CURRENT_RUNTIME.set(&rt, || while rt.poll() == PollResult::PollAgain {});
        assert_eq!(*done.borrow(), 1);
    }

    #[test]
    fn event_interval() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for val in [0, 1] {
            let rt = Runtime::builder()
                .event_interval(val)
                .build(Box::new(NoopNotify));
            assert_eq!(rt.poll(), PollResult::Pending);

            rt.spawn(async {}).detach();
            rt.spawn(async {}).detach();
            assert_eq!(rt.poll(), PollResult::PollAgain);
            assert_eq!(rt.poll(), PollResult::Pending);
        }

        // tasks scheduled from other threads overflow the fixed queue
        let rt = Runtime::builder()
            .event_interval(1)
            .build(Box::new(NoopNotify));
        let cnt = Arc::new(AtomicUsize::new(0));
        let hnd = rt.handle();
        let cnt2 = cnt.clone();
        std::thread::spawn(move || {
            for _ in 0..130 {
                let cnt = cnt2.clone();
                hnd.spawn(async move {
                    cnt.fetch_add(1, Ordering::Relaxed);
                })
                .detach();
            }
        })
        .join()
        .unwrap();
        assert_eq!(rt.poll(), PollResult::PollAgain);
        assert_eq!(cnt.load(Ordering::Relaxed), 129);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert_eq!(cnt.load(Ordering::Relaxed), 130);
    }

    #[test]
    fn now() {
        let rt = Runtime::new(Box::new(NoopNotify));

        // idle runtime returns the current time
        let t = Instant::now();
        assert!(rt.now() >= t);
        std::thread::sleep(Duration::from_millis(5));
        assert!(rt.now() >= t + Duration::from_millis(5));

        // tasks observe the turn start
        rt.awake();
        let start = rt.now();
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(rt.now(), start);

        let seen = Rc::new(RefCell::new(Vec::new()));
        for _ in 0..2 {
            let seen = seen.clone();
            rt.spawn(async move {
                seen.borrow_mut().push(Runtime::with_current(Runtime::now));
                std::thread::sleep(Duration::from_millis(5));
            })
            .detach();
        }
        CURRENT_RUNTIME.set(&rt, || assert_eq!(rt.poll(), PollResult::Pending));
        assert_eq!(*seen.borrow(), vec![start, start]);

        // idle again after the queue is drained
        assert!(rt.now() > start);
    }

    #[test]
    fn local_schedule_while_idle_starts_turn() {
        let sleep = Duration::from_millis(20);
        let rt = Runtime::new(Box::new(NoopNotify));
        rt.awake();
        let start = rt.now();
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        let busy = rt.busy_duration();

        // the driver wakes a task before blocking, the next turn starts here
        rt.spawn(async {}).detach();
        rt.awake();
        assert!(rt.now() >= start + sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(rt.busy_duration() < busy + sleep);
    }

    #[test]
    fn busy_duration() {
        let sleep = Duration::from_millis(20);
        let rt = Runtime::builder()
            .event_interval(1)
            .build(Box::new(NoopNotify));
        let hnd = rt.handle();
        assert_eq!(rt.poll(), PollResult::Pending);

        // task time is counted
        rt.spawn(async move { std::thread::sleep(sleep) }).detach();
        assert_eq!(rt.poll(), PollResult::Pending);
        let busy = hnd.busy_duration().unwrap();
        assert!(busy >= sleep);

        // time between `awake` and `poll` is counted
        rt.awake();
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(hnd.busy_duration().unwrap() >= busy + sleep);

        // idle time is not counted
        let busy = rt.busy_duration();
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(rt.busy_duration() < busy + sleep);

        // published on each poll while the runtime stays busy
        rt.spawn(async move { std::thread::sleep(sleep) }).detach();
        rt.spawn(async {}).detach();
        let busy = rt.busy_duration();
        assert_eq!(rt.poll(), PollResult::PollAgain);
        let busy2 = rt.busy_duration();
        assert!(busy2 >= busy + sleep);

        // time between busy polls is counted
        std::thread::sleep(sleep);
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(rt.busy_duration() >= busy2 + sleep);
    }

    #[test]
    fn busy_duration_during_turn() {
        let sleep = Duration::from_millis(50);
        let (tx, rx) = std::sync::mpsc::channel();
        let th = thread::spawn(move || {
            let rt = Runtime::new(Box::new(NoopNotify));
            let hnd = rt.handle();
            rt.spawn(async move {
                tx.send(hnd).unwrap();
                thread::sleep(sleep);
            })
            .detach();
            assert_eq!(rt.poll(), PollResult::Pending);
            rt
        });

        // the task is running, its turn is visible from another thread
        let hnd = rx.recv().unwrap();
        thread::sleep(sleep / 2);
        assert!(hnd.busy_duration().unwrap() >= sleep / 2);
        assert!(hnd.current_turn().unwrap() >= sleep / 2);

        let rt = th.join().unwrap();
        assert!(hnd.busy_duration().unwrap() >= sleep);
        assert_eq!(hnd.current_turn(), None);
        assert_eq!(rt.current_turn(), None);
    }

    #[test]
    fn busy_duration_is_monotonic() {
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let done2 = done.clone();
        let th = thread::spawn(move || {
            let rt = Runtime::new(Box::new(NoopNotify));
            tx.send(rt.handle()).unwrap();
            for _ in 0..100 {
                rt.spawn(async { thread::sleep(Duration::from_micros(500)) })
                    .detach();
                assert_eq!(rt.poll(), PollResult::Pending);
                thread::sleep(Duration::from_micros(200));
            }
            done2.store(true, Ordering::Release);
        });

        // a busy period is not counted twice when it ends
        let hnd = rx.recv().unwrap();
        let mut last = Duration::ZERO;
        while !done.load(Ordering::Acquire) {
            let busy = hnd.busy_duration().unwrap();
            assert!(
                busy + Duration::from_micros(100) >= last,
                "{busy:?} < {last:?}"
            );
            last = last.max(busy);
        }
        th.join().unwrap();
    }

    #[test]
    fn schedule_while_clearing() {
        let rt = Runtime::new(Box::new(NoopNotify));
        let waker = Rc::new(RefCell::new(None));
        let waker2 = waker.clone();
        rt.spawn(poll_fn(move |cx| {
            *waker2.borrow_mut() = Some(cx.waker().clone());
            Poll::<()>::Pending
        }))
        .detach();
        assert_eq!(rt.poll(), PollResult::Pending);
        assert!(waker.borrow().is_some());

        // queued task wakes the waiting task while the queue is cleared
        let guard = WakeOnDrop(waker.clone());
        rt.spawn(async move {
            let _g = guard;
        })
        .detach();
        rt.spawn(async {}).detach();
        drop(rt);
        assert!(waker.borrow().is_none());
        // the woken task is dropped too, releasing its clone of `waker`
        assert_eq!(Rc::strong_count(&waker), 1);
    }
}
