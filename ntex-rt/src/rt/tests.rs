use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::{cell::Cell, cell::RefCell, future::poll_fn, rc::Rc, thread};

use super::*;

/// Iterations of a stress test, fewer under Miri.
const fn iters(n: usize, miri: usize) -> usize {
    if cfg!(miri) { miri } else { n }
}

/// Wall clock limit of a wait, Miri runs much slower.
const WAIT: Duration = Duration::from_secs(if cfg!(miri) { 600 } else { 30 });

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

#[derive(Debug)]
struct FailNotify {
    calls: Arc<AtomicUsize>,
    failures: usize,
    kind: io::ErrorKind,
}

impl Notify for FailNotify {
    fn notify(&self) -> io::Result<()> {
        if self.calls.fetch_add(1, Ordering::Relaxed) < self.failures {
            Err(self.kind.into())
        } else {
            Ok(())
        }
    }
}

struct PollDriver;

impl Driver for PollDriver {
    fn handle(&self) -> Box<dyn Notify> {
        Box::new(NoopNotify)
    }

    fn run(&self, rt: &Runtime) -> io::Result<()> {
        while rt.poll() != PollResult::Ready {}
        Ok(())
    }
}

#[test]
fn interrupted_scheduler_notifications() {
    for remote in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        let rt = Runtime::new(Box::new(FailNotify {
            calls: calls.clone(),
            failures: 2,
            kind: io::ErrorKind::Interrupted,
        }));
        for round in 0..2 {
            let (handle, done2) = (rt.handle(), done.clone());
            let submit = move || {
                for _ in 0..2 {
                    let done = done2.clone();
                    handle.spawn_detached(async move {
                        done.fetch_add(1, Ordering::Relaxed);
                    });
                }
            };
            if remote {
                thread::spawn(submit).join().unwrap();
            } else {
                submit();
            }
            assert_eq!(calls.load(Ordering::Relaxed), 3 + round);
            assert_eq!(rt.poll(), PollResult::Pending);
            assert_eq!(done.load(Ordering::Relaxed), 2 * (round + 1));
        }
    }
}

#[test]
fn interrupted_block_on_notification() {
    let calls = Arc::new(AtomicUsize::new(0));
    let rt = Runtime::new(Box::new(FailNotify {
        calls: calls.clone(),
        failures: 2,
        kind: io::ErrorKind::Interrupted,
    }));
    rt.awake();
    assert_eq!(rt.block_on(async { 42 }, &PollDriver), 42);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[test]
fn handle_notify_returns_errors() {
    for kind in [io::ErrorKind::Interrupted, io::ErrorKind::Other] {
        let calls = Arc::new(AtomicUsize::new(0));
        let rt = Runtime::new(Box::new(FailNotify {
            calls: calls.clone(),
            failures: usize::MAX,
            kind,
        }));
        assert_eq!(rt.handle().notify().unwrap_err().kind(), kind);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}

#[cfg(not(any(miri, ntex_loom)))]
#[test]
fn failed_scheduler_notification_aborts() {
    use std::process::{Command, Stdio};

    const CHILD: &str = "NTEX_RT_NOTIFY_ABORT_CHILD";
    if let Some(path) = std::env::var_os(CHILD) {
        struct Logger;
        impl log::Log for Logger {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                eprintln!("{}", record.args());
            }
            fn flush(&self) {}
        }
        log::set_logger(&Logger).unwrap();
        log::set_max_level(log::LevelFilter::Error);
        let rt = Runtime::new(Box::new(FailNotify {
            calls: Arc::new(AtomicUsize::new(0)),
            failures: usize::MAX,
            kind: io::ErrorKind::Other,
        }));
        match path.to_str().unwrap() {
            "local" => rt.spawn_detached(async {}),
            "remote" => {
                let handle = rt.handle();
                thread::spawn(move || handle.spawn_detached(async {}))
                    .join()
                    .unwrap();
            }
            "block_on" => {
                rt.awake();
                rt.block_on(async {}, &PollDriver);
            }
            _ => panic!("unknown notification path"),
        }
        panic!("notification failure did not abort");
    }

    for path in ["local", "remote", "block_on"] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rt::tests::failed_scheduler_notification_aborts",
                "--nocapture",
            ])
            .env(CHILD, path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > WAIT {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{path}: notification failure did not terminate the process");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Failed to notify runtime driver:"),
            "{stderr}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{stderr}");
        }
        #[cfg(not(unix))]
        {
            assert!(!output.status.success(), "{stderr}");
            assert_ne!(output.status.code(), Some(101), "{stderr}");
        }
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
    const TASKS: usize = iters(10_000, 50);
    const LOST: Duration = if cfg!(miri) { WAIT } else { Duration::from_secs(5) };

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
            // the last tasks may have run in this poll
            let (mut guard, res) =
                ev.1.wait_timeout_while(guard, LOST, |n| {
                    !*n && cnt.load(Ordering::Relaxed) < THREADS * TASKS
                })
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
#[cfg(not(ntex_loom))]
fn runtime_spawn_skips_owner_lookup() {
    let borrowed = Cell::new(false);
    let rt = Runtime::new(Box::new(NoopNotify));
    let output = Rc::new(42);
    let detached = Rc::new(Cell::new(false));
    let done = detached.clone();

    // All Runtime entrypoints already know the owner, even with a cold TLS cache.
    let id = THREAD_ID.with(Cell::take);
    let task = rt.spawn(async move { output });
    let unchecked = unsafe { rt.spawn_unchecked(async { borrowed.set(true) }) };
    rt.spawn_detached(async move { done.set(true) });
    assert!(THREAD_ID.with(Cell::get).is_none());
    THREAD_ID.with(|slot| slot.set(id));

    assert_eq!(rt.poll(), PollResult::Pending);
    assert_eq!(*block_on_thread(task).unwrap(), 42);
    block_on_thread(unchecked).unwrap();
    assert!(borrowed.get());
    assert!(detached.get());
    assert_eq!(Arc::strong_count(&rt.queue), 1, "tasks freed");
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
    // the runtime is not current on this thread
    assert!(crate::rt_default::now() >= start + Duration::from_millis(5));

    let seen = Rc::new(RefCell::new(Vec::new()));
    for _ in 0..2 {
        let seen = seen.clone();
        rt.spawn(async move {
            seen.borrow_mut().push((
                Runtime::with_current(Runtime::now),
                crate::rt_default::now(),
            ));
            std::thread::sleep(Duration::from_millis(5));
        })
        .detach();
    }
    CURRENT_RUNTIME.set(&rt, || assert_eq!(rt.poll(), PollResult::Pending));
    assert_eq!(*seen.borrow(), vec![(start, start), (start, start)]);

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
    let turn_start = rt.now();
    rt.awake();
    assert_eq!(rt.now(), turn_start);
    assert!(turn_start >= start + sleep);
    let before_poll = turn_start.elapsed();
    assert_eq!(rt.poll(), PollResult::Pending);
    let after_poll = turn_start.elapsed();
    let total = rt.busy_duration();
    assert!(total >= busy + before_poll);
    assert!(total <= busy + after_poll);
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
    assert_eq!(rt.busy_duration(), busy);
    let poll_start = Instant::now();
    assert_eq!(rt.poll(), PollResult::Pending);
    let poll_elapsed = poll_start.elapsed();
    assert!(rt.busy_duration() <= busy + poll_elapsed);

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
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let th = thread::spawn(move || {
        let rt = Runtime::new(Box::new(NoopNotify));
        let hnd = rt.handle();
        rt.spawn(async move {
            tx.send(hnd).unwrap();
            thread::sleep(sleep);
            // keep the turn running until the other thread checked it
            let _ = done_rx.recv();
        })
        .detach();
        assert_eq!(rt.poll(), PollResult::Pending);
        assert_eq!(rt.current_turn(), None);
    });

    // the task is running, its turn is visible from another thread
    let hnd = rx.recv().unwrap();
    thread::sleep(sleep / 2);
    assert!(hnd.busy_duration().unwrap() >= sleep / 2);
    assert!(hnd.current_turn().unwrap() >= sleep / 2);
    done_tx.send(()).unwrap();

    th.join().unwrap();
    assert!(hnd.busy_duration().unwrap() >= sleep);
    assert_eq!(hnd.current_turn(), None);
}

#[test]
fn current_task() {
    let (tx, rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (polled_tx, polled_rx) = std::sync::mpsc::channel::<()>();
    let (spawned_tx, spawned_rx) = std::sync::mpsc::channel::<()>();
    let th = thread::spawn(move || {
        let rt = Runtime::new(Box::new(NoopNotify));
        let hnd = rt.handle();
        assert_eq!(hnd.current_task(), None);

        let line = line!() + 1;
        rt.spawn(async move {
            tx.send((hnd, line)).unwrap();
            release_rx.recv().unwrap();
        })
        .detach();
        assert_eq!(rt.poll(), PollResult::Pending);
        polled_tx.send(()).unwrap();

        spawned_rx.recv().unwrap();
        assert_eq!(rt.poll(), PollResult::Pending);
    });

    // the running task is visible from another thread
    let (hnd, line) = rx.recv().unwrap();
    let task = hnd.current_task().unwrap();
    assert_eq!((task.file(), task.line()), (file!(), line));
    release_tx.send(()).unwrap();

    polled_rx.recv().unwrap();
    assert_eq!(hnd.current_task(), None);

    // tasks spawned from other threads record their location too
    let (tx, rx) = std::sync::mpsc::channel();
    let hnd2 = hnd.clone();
    let line = line!() + 1;
    hnd.spawn(async move {
        tx.send(hnd2.current_task()).unwrap();
    })
    .detach();
    spawned_tx.send(()).unwrap();
    th.join().unwrap();
    let task = rx.recv().unwrap().unwrap();
    assert_eq!((task.file(), task.line()), (file!(), line));
    assert_eq!(hnd.current_task(), None);
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
        assert!(busy >= last, "{busy:?} < {last:?}");
        last = busy;
    }
    th.join().unwrap();
}

#[test]
fn poll_count() {
    let rt = Runtime::builder()
        .event_interval(1)
        .build(Box::new(NoopNotify));
    let hnd = rt.handle();
    assert_eq!((rt.poll_count(), rt.poll_again_count()), (0, 0));

    assert_eq!(rt.poll(), PollResult::Pending);
    assert_eq!((rt.poll_count(), rt.poll_again_count()), (1, 0));

    // one task per poll, the first poll leaves a backlog
    rt.spawn(async {}).detach();
    rt.spawn(async {}).detach();
    assert_eq!(rt.poll(), PollResult::PollAgain);
    assert_eq!(rt.poll(), PollResult::Pending);
    assert_eq!(hnd.poll_count(), Some(3));
    assert_eq!(hnd.poll_again_count(), Some(1));
    assert_eq!(hnd.poll_histogram().unwrap().count(), 3);

    // a stopped runtime does not run tasks
    rt.stop.set(true);
    assert_eq!(rt.poll(), PollResult::Ready);
    assert_eq!((rt.poll_count(), rt.poll_again_count()), (3, 1));
    assert_eq!(rt.poll_histogram().count(), 3);
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

#[test]
fn self_wake() {
    for count in [0, 10_000] {
        let rt = Runtime::builder()
            .self_wake_threshold(count, Duration::from_secs(1))
            .build(Box::new(NoopNotify));
        let waker = Rc::new(RefCell::new(None::<Waker>));
        let waker2 = waker.clone();
        let mut polls = 0;
        let h = rt.spawn(poll_fn(move |cx| {
            polls += 1;
            match polls {
                1..=5 => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                6 => {
                    *waker2.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                }
                _ => Poll::Ready(()),
            }
        }));
        CURRENT_RUNTIME.set(&rt, || while rt.poll() == PollResult::PollAgain {});
        assert_eq!(rt.self_wake_count(), 5);
        assert_eq!(rt.handle().self_wake_count(), Some(5));
        if count == 0 {
            assert_eq!(rt.queue.self_wake_clock.get(), None);
        }

        // a wake from outside of the poll is not a self-wake
        waker.borrow_mut().take().unwrap().wake();
        CURRENT_RUNTIME.set(&rt, || while rt.poll() == PollResult::PollAgain {});
        assert!(h.is_finished());
        assert_eq!(rt.self_wake_count(), 5);
    }
}

#[test]
fn self_wake_streak() {
    #[derive(Default)]
    struct Stats {
        wakes: Cell<u32>,
        start: Cell<u32>,
    }

    impl TaskStats for &Stats {
        fn location(self) -> SpawnLocation {
            Location::caller()
        }
        fn self_wakes(self) -> u32 {
            self.wakes.get()
        }
        fn set_self_wakes(self, wakes: u32) {
            self.wakes.set(wakes);
        }
        fn streak_start(self) -> u32 {
            self.start.get()
        }
        fn set_streak_start(self, start: u32) {
            self.start.set(start);
        }
    }

    fn streak(rt: &Runtime, n: u32) -> u32 {
        let stats = Stats::default();
        for _ in 0..n {
            rt.queue.self_wake(&stats, stats.wakes.get());
        }
        stats.wakes.get()
    }

    fn start(rt: &Runtime) -> u32 {
        let stats = Stats::default();
        rt.queue.self_wake(&stats, 0);
        stats.start.get()
    }

    fn now(rt: &Runtime) -> u32 {
        u32::try_from(
            rt.queue
                .poll_start
                .get()
                .saturating_duration_since(rt.queue.epoch)
                .as_millis(),
        )
        .unwrap()
    }

    struct BusyDriver;

    impl Driver for BusyDriver {
        fn handle(&self) -> Box<dyn Notify> {
            Box::new(NoopNotify)
        }

        fn run(&self, rt: &Runtime) -> io::Result<()> {
            let start = Instant::now();
            while rt.poll() != PollResult::Ready {
                assert!(start.elapsed() < WAIT, "timeout");
            }
            Ok(())
        }
    }

    let rt = Runtime::builder()
        .self_wake_threshold(3, Duration::ZERO)
        .build(Box::new(NoopNotify));
    assert_eq!(streak(&rt, 2), 2);
    assert_eq!(streak(&rt, 3), 3 | REPORTED);
    assert_eq!(streak(&rt, 5), 5 | REPORTED);
    assert_eq!(rt.self_wake_count(), 10);

    // the streak is shorter than the duration
    let rt = Runtime::builder()
        .self_wake_threshold(3, Duration::from_hours(1))
        .build(Box::new(NoopNotify));
    assert_eq!(streak(&rt, 5), 5);

    // disabled
    let rt = Runtime::builder()
        .self_wake_threshold(0, Duration::ZERO)
        .build(Box::new(NoopNotify));
    assert_eq!(streak(&rt, 5), 0);
    assert_eq!(rt.self_wake_count(), 5);

    let rt = Runtime::new(Box::new(NoopNotify));
    let before = start(&rt);
    thread::sleep(Duration::from_millis(2));
    rt.awake();
    assert!(now(&rt) > before);
    assert_eq!(start(&rt), now(&rt), "streak after awake");

    let task = rt.spawn(poll_fn(|cx| {
        cx.waker().wake_by_ref();
        Poll::<()>::Pending
    }));
    let before = start(&rt);
    thread::sleep(Duration::from_millis(2));
    assert_eq!(rt.poll(), PollResult::PollAgain);
    assert!(now(&rt) > before);
    assert_eq!(start(&rt), now(&rt), "streak after the next poll");
    task.cancel();
    assert_eq!(rt.poll(), PollResult::Pending);

    // Already awake, so the root spawn does not refresh the clock.
    rt.awake();
    let before = start(&rt);
    thread::sleep(Duration::from_millis(2));
    rt.block_on(
        async {
            assert!(now(&rt) > before);
            assert_eq!(start(&rt), now(&rt), "streak in a new block_on");
        },
        &BusyDriver,
    );
}

#[test]
fn reentrant_self_wake_logger() {
    scoped_tls::scoped_thread_local!(static WARNINGS: Cell<usize>);

    struct Logger;

    impl log::Log for Logger {
        fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
            metadata.level() == log::Level::Warn
                && metadata.target() == "ntex_rt::rt"
                && WARNINGS.is_set()
        }

        fn log(&self, record: &log::Record<'_>) {
            if self.enabled(record.metadata()) {
                WARNINGS.with(|warnings| {
                    let count = warnings.get();
                    warnings.set(count + 1);
                    if count == 0 {
                        CURRENT_RUNTIME.with(|rt| {
                            rt.poll();
                        });
                    }
                });
            }
        }

        fn flush(&self) {}
    }

    log::set_logger(&Logger).unwrap();
    log::set_max_level(log::LevelFilter::Warn);

    // The nested poll either wakes again or completes and frees the task.
    for ready_at in [3, 2] {
        let rt = Runtime::builder()
            .event_interval(1)
            .self_wake_threshold(1, Duration::ZERO)
            .build(Box::new(NoopNotify));
        let warnings = Cell::new(0);
        let polls = Rc::new(Cell::new(0u32));
        let count = polls.clone();
        rt.spawn_detached(poll_fn(move |cx| {
            let n = count.get() + 1;
            count.set(n);
            if n < ready_at {
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));

        CURRENT_RUNTIME.set(&rt, || {
            WARNINGS.set(&warnings, || {
                rt.poll();
                assert_eq!(warnings.get(), 1, "one warning per streak");
                assert_eq!(rt.poll(), PollResult::Pending);
            });
        });
        assert_eq!(polls.get(), ready_at);
        assert_eq!(rt.self_wake_count(), u64::from(ready_at - 1));
        assert_eq!(Arc::strong_count(&rt.queue), 1, "task freed");
    }
}

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

type Slot = Arc<std::sync::Mutex<Option<Waker>>>;

fn poll_until(rt: &Runtime, cond: impl Fn() -> bool) {
    let start = Instant::now();
    CURRENT_RUNTIME.set(rt, || {
        while !cond() {
            assert!(start.elapsed() < WAIT, "timeout");
            if rt.poll() == PollResult::Pending {
                thread::yield_now();
            }
        }
    });
}

fn block_on_thread<F: Future>(fut: F) -> F::Output {
    struct ThreadWaker(thread::Thread);

    impl std::task::Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        thread::park();
    }
}

/// Stores its waker until polled `n` times.
fn wait_polls(
    slot: Slot,
    polls: Arc<std::sync::atomic::AtomicUsize>,
    n: usize,
) -> impl Future<Output = usize> {
    poll_fn(move |cx| {
        let cnt = polls.fetch_add(1, Ordering::SeqCst) + 1;
        if cnt >= n {
            Poll::Ready(cnt)
        } else {
            *slot.lock().unwrap() = Some(cx.waker().clone());
            Poll::Pending
        }
    })
}

#[test]
fn remote_wakers() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let slot = Slot::default();
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = rt.spawn(wait_polls(slot.clone(), polls.clone(), 4));
    poll_until(&rt, || polls.load(Ordering::SeqCst) == 1);

    // wake by ref, the thread keeps the waker
    let w = slot.lock().unwrap().take().unwrap();
    let th = thread::spawn(move || {
        w.wake_by_ref();
        w
    });
    let w = th.join().unwrap();
    poll_until(&rt, || polls.load(Ordering::SeqCst) == 2);

    // clone, drop and wake by value
    let w2 = slot.lock().unwrap().take().unwrap();
    thread::spawn(move || {
        let c = w2.clone();
        drop(w2);
        drop(w);
        c.wake();
    })
    .join()
    .unwrap();
    poll_until(&rt, || polls.load(Ordering::SeqCst) == 3);

    // joined on another thread while the runtime polls
    let w = slot.lock().unwrap().take().unwrap();
    let th = thread::spawn(move || {
        w.wake();
        block_on_thread(h)
    });
    poll_until(&rt, || th.is_finished());
    assert_eq!(th.join().unwrap().unwrap(), 4);
}

#[test]
fn remote_wakers_stress() {
    const POLLS: usize = iters(5000, 50);

    let rt = Runtime::new(Box::new(NoopNotify));
    let slot = Slot::default();
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let h = rt.spawn(wait_polls(slot.clone(), polls.clone(), POLLS));

    let wakers: Vec<_> = (0..4)
        .map(|i| {
            let slot = slot.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let w = slot.lock().unwrap().clone();
                    match (w, i % 3) {
                        (Some(w), 0) => w.wake(),
                        (Some(w), 1) => w.wake_by_ref(),
                        (Some(w), _) => drop(w.clone()),
                        (None, _) => thread::yield_now(),
                    }
                }
            })
        })
        .collect();
    let joiner = thread::spawn(move || block_on_thread(h));

    poll_until(&rt, || joiner.is_finished());
    stop.store(true, Ordering::SeqCst);
    for th in wakers {
        th.join().unwrap();
    }
    assert_eq!(joiner.join().unwrap().unwrap(), POLLS);
    assert_eq!(polls.load(Ordering::SeqCst), POLLS);
    poll_until(&rt, || true);
}

#[test]
fn last_waker_drops_future() {
    let rt = Runtime::new(Box::new(NoopNotify));
    for remote in [false, true] {
        let slot = Slot::default();
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        let slot2 = slot.clone();
        rt.spawn(async move {
            let _flag = flag;
            poll_fn(|cx| {
                *slot2.lock().unwrap() = Some(cx.waker().clone());
                Poll::<()>::Pending
            })
            .await;
        })
        .detach();
        poll_until(&rt, || slot.lock().unwrap().is_some());
        assert!(!dropped.load(Ordering::SeqCst));

        let w = slot.lock().unwrap().take();
        if remote {
            thread::spawn(move || drop(w)).join().unwrap();
        } else {
            drop(w);
        }
        poll_until(&rt, || dropped.load(Ordering::SeqCst));
    }
}

#[test]
fn remote_cancel() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let polled = Rc::new(Cell::new(false));
    let polled2 = polled.clone();
    let h = rt.spawn(async move {
        let _flag = flag;
        polled2.set(true);
        poll_fn(|_| Poll::<()>::Pending).await;
    });
    poll_until(&rt, || polled.get());

    thread::spawn(move || h.cancel()).join().unwrap();
    poll_until(&rt, || dropped.load(Ordering::SeqCst));
}

#[test]
fn cancel_while_running() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let handle = Rc::new(RefCell::new(None::<JoinHandle<()>>));
    let handle2 = handle.clone();
    let polls = Rc::new(Cell::new(0));
    let polls2 = polls.clone();
    let h = rt.spawn(async move {
        let _flag = flag;
        poll_fn(|cx| {
            polls2.set(polls2.get() + 1);
            if let Some(h) = handle2.borrow_mut().take() {
                cx.waker().wake_by_ref();
                h.cancel();
            }
            Poll::<()>::Pending
        })
        .await;
    });
    *handle.borrow_mut() = Some(h);
    poll_until(&rt, || dropped.load(Ordering::SeqCst));
    assert_eq!(polls.get(), 1);
}

#[test]
fn panic_in_poll() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(dropped.clone());
    let h = rt.spawn(async move {
        let _flag = flag;
        panic!("task panic");
    });
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        CURRENT_RUNTIME.set(&rt, || rt.poll())
    }));
    assert!(res.is_err());
    assert!(dropped.load(Ordering::SeqCst));
    assert!(h.is_finished());
    assert!(block_on_thread(h).is_err());

    // the runtime keeps running tasks
    let done = Rc::new(Cell::new(false));
    let done2 = done.clone();
    rt.spawn(async move { done2.set(true) }).detach();
    poll_until(&rt, || done.get());
}

#[test]
fn drop_runtime_with_tasks() {
    let dropped = Arc::new(AtomicBool::new(false));
    let remote = Arc::new(AtomicBool::new(false));
    let rt = Runtime::new(Box::new(NoopNotify));
    let flag = DropFlag(dropped.clone());
    let h = rt.spawn(async move {
        let _flag = flag;
        poll_fn(|_| Poll::<()>::Pending).await;
    });
    let flag = DropFlag(remote.clone());
    let hnd = rt.handle();
    thread::spawn(move || {
        hnd.spawn(async move {
            let _flag = flag;
        })
        .detach();
    })
    .join()
    .unwrap();
    drop(rt);
    assert!(dropped.load(Ordering::SeqCst));
    assert!(remote.load(Ordering::SeqCst));
    assert!(h.is_finished());
}

/// Pending future that borrows the caller's frame and sets a flag on drop.
struct Borrowing<'a>(&'a Cell<bool>, Option<Waker>);

impl Future for Borrowing<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        assert!(!self.0.get());
        self.1 = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Borrowing<'_> {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

/// A task that polls the runtime and wakes itself is not run by a notify
/// that reenters the runtime, the running poll picks it up.
#[test]
fn reentrant_notify_self_wake() {
    #[derive(Debug, Default)]
    struct ReenterNotify(AtomicUsize);

    impl Notify for ReenterNotify {
        fn notify(&self) -> io::Result<()> {
            if CURRENT_RUNTIME.is_set() {
                CURRENT_RUNTIME.with(|rt| {
                    if !rt.queue.current.load(Ordering::Relaxed).is_null() {
                        self.0.fetch_add(1, Ordering::Relaxed);
                    }
                    rt.poll();
                });
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    struct Shared(Arc<ReenterNotify>);

    impl Notify for Shared {
        fn notify(&self) -> io::Result<()> {
            self.0.notify()
        }
    }

    let notify = Arc::new(ReenterNotify::default());
    let rt = Runtime::new(Box::new(Shared(notify.clone())));
    let polls = Rc::new(Cell::new(0));
    let p = polls.clone();
    rt.spawn_detached(poll_fn(move |cx| {
        p.set(p.get() + 1);
        if p.get() == 1 {
            CURRENT_RUNTIME.with(|rt| {
                rt.poll();
            });
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }));
    CURRENT_RUNTIME.set(&rt, || {
        rt.poll();
        rt.poll();
    });
    assert_eq!(polls.get(), 2);
    assert_eq!(notify.0.load(Ordering::Relaxed), 0);
    assert_eq!(rt.self_wake_count(), 1);
    assert!(rt.queue.current.load(Ordering::Relaxed).is_null());
}

// Backtraces captured in a task stop at the task poll.
#[cfg(not(miri))]
#[test]
fn block_on_backtrace_cutoff() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let repr = rt.block_on(
        async { ntex_error::Backtrace::with_current().resolve().repr() },
        &PollDriver,
    );
    let repr = repr.unwrap();
    assert!(!repr.contains("task.rs"), "{repr}");
}

#[test]
fn block_on_rejects_nesting() {
    struct BusyDriver;

    impl Driver for BusyDriver {
        fn handle(&self) -> Box<dyn Notify> {
            Box::new(NoopNotify)
        }

        fn run(&self, rt: &Runtime) -> io::Result<()> {
            while rt.poll() != PollResult::Ready {}
            Ok(())
        }
    }

    for same in [true, false] {
        let rt = Runtime::new(Box::new(NoopNotify));
        let nested = rt.block_on(
            async {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if same {
                        Runtime::with_current(|rt| rt.block_on(async {}, &BusyDriver));
                    } else {
                        Runtime::new(Box::new(NoopNotify)).block_on(async {}, &BusyDriver);
                    }
                }))
                .is_err()
            },
            &BusyDriver,
        );
        assert!(nested);
        // the runtime is usable after the rejected call
        assert_eq!(rt.block_on(async { 1 }, &BusyDriver), 1);
    }
}

#[test]
fn block_on_drops_future_on_driver_error() {
    struct ErrorDriver;

    impl Driver for ErrorDriver {
        fn handle(&self) -> Box<dyn Notify> {
            Box::new(NoopNotify)
        }

        fn run(&self, _: &Runtime) -> io::Result<()> {
            Err(io::Error::other("driver stopped"))
        }
    }

    let rt = Runtime::new(Box::new(NoopNotify));
    let dropped = Cell::new(false);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(Borrowing(&dropped, None), &ErrorDriver);
    }));
    assert!(res.is_err());
    assert!(dropped.get());
    rt.poll();
}

#[test]
fn block_on_drops_future_on_driver_panic() {
    struct PanicDriver;

    impl Driver for PanicDriver {
        fn handle(&self) -> Box<dyn Notify> {
            Box::new(NoopNotify)
        }

        fn run(&self, rt: &Runtime) -> io::Result<()> {
            rt.poll();
            panic!("driver panic");
        }
    }

    let rt = Runtime::new(Box::new(NoopNotify));
    let dropped = Cell::new(false);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(Borrowing(&dropped, None), &PanicDriver);
    }));
    assert!(res.is_err());
    assert!(dropped.get());
    rt.poll();
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

#[derive(Default)]
struct CountWaker(AtomicUsize);

impl std::task::Wake for CountWaker {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn drain(rt: &Runtime) {
    CURRENT_RUNTIME.set(rt, || while rt.poll() == PollResult::PollAgain {});
}

async fn yield_once() {
    let mut yielded = false;
    poll_fn(|cx| {
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
fn remote_join_race() {
    const N: usize = iters(2000, 50);

    let rt = Runtime::new(Box::new(NoopNotify));
    let (tx, rx) = std::sync::mpsc::channel::<JoinHandle<usize>>();
    let done = Counter::default();
    let done2 = done.clone();
    let th = thread::spawn(move || {
        for (i, h) in rx.into_iter().enumerate() {
            assert_eq!(block_on_thread(h).unwrap(), i);
            done2.inc();
        }
    });
    for i in 0..N {
        // completes while the joining thread registers its waker
        let h = rt.spawn(async move {
            for _ in 0..i % 4 {
                yield_once().await;
            }
            i
        });
        tx.send(h).unwrap();
        poll_until(&rt, || done.get() == i + 1);
    }
    drop(tx);
    th.join().unwrap();
}

#[test]
fn unconsumed_output_dropped() {
    let rt = Runtime::new(Box::new(NoopNotify));
    for remote in [false, true] {
        let out = Counter::default();
        let tok = out.token();
        let h = rt.spawn(async move { tok });
        poll_until(&rt, || h.is_finished());
        assert_eq!(out.get(), 0);
        if remote {
            thread::spawn(move || drop(h)).join().unwrap();
        } else {
            drop(h);
        }
        assert_eq!(out.get(), 1);
    }

    // detached before completion
    let out = Counter::default();
    let tok = out.token();
    rt.spawn(async move {
        yield_once().await;
        tok
    })
    .detach();
    poll_until(&rt, || out.get() == 1);

    // consumed by the join handle
    let out = Counter::default();
    let tok = out.token();
    let h = rt.spawn(async move { tok });
    poll_until(&rt, || h.is_finished());
    drop(block_on_thread(h).unwrap());
    drain(&rt);
    assert_eq!(out.get(), 1);
}

#[test]
fn remote_cancel_before_run() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let dropped = Arc::new(AtomicBool::new(false));
    let polled = Arc::new(AtomicBool::new(false));
    let hnd = rt.handle();
    let flag = DropFlag(dropped.clone());
    let polled2 = polled.clone();
    thread::spawn(move || {
        hnd.spawn(async move {
            let _flag = flag;
            polled2.store(true, Ordering::SeqCst);
        })
        .cancel();
    })
    .join()
    .unwrap();
    poll_until(&rt, || dropped.load(Ordering::SeqCst));
    assert!(!polled.load(Ordering::SeqCst));
}

#[test]
fn cancel_complete_race() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let created = Counter::default();
    let out = Counter::default();
    for _ in 0..iters(500, 10) {
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(dropped.clone());
        let slot = Slot::default();
        let (slot2, created2, out2) = (slot.clone(), created.clone(), out.clone());
        let h = rt.spawn(async move {
            let _flag = flag;
            let mut first = true;
            poll_fn(|cx| {
                if first {
                    first = false;
                    *slot2.lock().unwrap() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            created2.inc();
            out2.token()
        });
        poll_until(&rt, || slot.lock().unwrap().is_some());

        let w = slot.lock().unwrap().take().unwrap();
        let t1 = thread::spawn(move || w.wake());
        let t2 = thread::spawn(move || h.cancel());
        poll_until(&rt, || t1.is_finished() && t2.is_finished());
        t1.join().unwrap();
        t2.join().unwrap();
        // the output is dropped once, whichever side wins
        poll_until(&rt, || {
            dropped.load(Ordering::SeqCst) && created.get() == out.get()
        });
    }
}

#[test]
fn stale_wakers() {
    let rt = Runtime::new(Box::new(NoopNotify));
    let slot = Slot::default();
    let polls = Arc::new(AtomicUsize::new(0));
    let out = Counter::default();
    let tok = out.token();
    let (slot2, polls2) = (slot.clone(), polls.clone());
    let h = rt.spawn(async move {
        wait_polls(slot2, polls2, 2).await;
        tok
    });
    poll_until(&rt, || polls.load(Ordering::SeqCst) == 1);

    let w = slot.lock().unwrap().take().unwrap();
    let (w2, w3) = (w.clone(), w.clone());
    w.wake();
    poll_until(&rt, || h.is_finished());

    // wakes after completion do not run the task
    w2.wake_by_ref();
    let w3 = thread::spawn(move || {
        w3.wake_by_ref();
        w3
    })
    .join()
    .unwrap();
    drain(&rt);
    assert_eq!(polls.load(Ordering::SeqCst), 2);

    // the output is dropped with the join handle
    drop(h);
    assert_eq!(out.get(), 1);
    // the last waker, dropped on another thread, frees the task
    drop(w2);
    thread::spawn(move || w3.wake()).join().unwrap();
    drain(&rt);
}

#[test]
fn unclaimed_output_with_own_waker() {
    // the output holds a waker of its task, a cycle unless it is dropped
    for case in 0..5 {
        let rt = Runtime::new(Box::new(NoopNotify));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut flag = Some(DropFlag(dropped.clone()));
        let h = rt.spawn(poll_fn(move |cx| {
            Poll::Ready((cx.waker().clone(), flag.take().unwrap()))
        }));
        let remote_drop = |h| thread::spawn(move || drop(h)).join().unwrap();
        match case {
            0 => h.detach(),
            1 => remote_drop(h),
            2 => {
                drain(&rt);
                h.detach();
            }
            3 => {
                drain(&rt);
                h.cancel();
            }
            _ => {
                drain(&rt);
                remote_drop(h);
            }
        }
        drain(&rt);
        assert!(dropped.load(Ordering::SeqCst), "case {case}");
    }
}

struct ReDrop {
    waker: Rc<RefCell<Option<Waker>>>,
    other: Option<JoinHandle<()>>,
    hnd: Handle,
    spawned: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Drop for ReDrop {
    fn drop(&mut self) {
        // wakes itself, drops a handle and spawns a task
        if let Some(w) = self.waker.borrow_mut().take() {
            w.wake();
        }
        drop(self.other.take());
        let flag = DropFlag(self.spawned.clone());
        self.hnd
            .spawn(async move {
                let _flag = flag;
            })
            .detach();
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[test]
fn reentrant_drop() {
    for clear in [false, true] {
        let rt = Runtime::new(Box::new(NoopNotify));
        let other_dropped = Arc::new(AtomicBool::new(false));
        let spawned = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = DropFlag(other_dropped.clone());
        let other = rt.spawn(async move {
            let _flag = flag;
            poll_fn(|_| Poll::<()>::Pending).await;
        });
        let waker = Rc::new(RefCell::new(None::<Waker>));
        let redrop = ReDrop {
            waker: waker.clone(),
            other: Some(other),
            hnd: rt.handle(),
            spawned: spawned.clone(),
            dropped: dropped.clone(),
        };
        let waker2 = waker.clone();
        let h = rt.spawn(async move {
            let _redrop = redrop;
            poll_fn(|cx| {
                *waker2.borrow_mut() = Some(cx.waker().clone());
                Poll::<()>::Pending
            })
            .await;
        });
        poll_until(&rt, || waker.borrow().is_some());

        if clear {
            // queued when the runtime is dropped
            h.detach();
            waker.borrow().as_ref().unwrap().wake_by_ref();
            drop(rt);
        } else {
            h.cancel();
            poll_until(&rt, || spawned.load(Ordering::SeqCst));
        }
        assert!(dropped.load(Ordering::SeqCst));
        assert!(other_dropped.load(Ordering::SeqCst));
        assert!(spawned.load(Ordering::SeqCst));
    }
}

/// Wakes its task and polls the runtime on drop.
struct PollOnDrop {
    waker: Rc<RefCell<Option<Waker>>>,
    drops: Counter,
}

impl Drop for PollOnDrop {
    fn drop(&mut self) {
        self.drops.inc();
        let waker = self.waker.borrow_mut().take();
        if let Some(w) = waker {
            w.wake();
        }
        Runtime::with_current(|rt| while rt.poll() == PollResult::PollAgain {});
    }
}

#[test]
fn reentrant_poll_in_drop() {
    for clear in [false, true] {
        let rt = Runtime::new(Box::new(NoopNotify));
        let drops = Counter::default();
        let polls = Counter::default();
        let waker = Rc::new(RefCell::new(None::<Waker>));
        let guard = PollOnDrop {
            waker: waker.clone(),
            drops: drops.clone(),
        };
        let (waker2, polls2) = (waker.clone(), polls.clone());
        let h = rt.spawn(async move {
            let _guard = guard;
            poll_fn(|cx| {
                polls2.inc();
                *waker2.borrow_mut() = Some(cx.waker().clone());
                Poll::<()>::Pending
            })
            .await;
        });
        poll_until(&rt, || waker.borrow().is_some());

        if clear {
            // queued when the runtime is dropped
            h.detach();
            waker.borrow().as_ref().unwrap().wake_by_ref();
            drop(rt);
        } else {
            h.cancel();
            drain(&rt);
        }
        // the future is neither polled nor dropped again
        assert_eq!(drops.get(), 1);
        assert_eq!(polls.get(), 1);
    }
}

/// Waker that runs a callback when it is cloned or released.
struct Hook {
    refs: Cell<usize>,
    on_clone: Cell<Option<Box<dyn FnOnce()>>>,
    on_release: Cell<Option<Box<dyn FnOnce()>>>,
}

impl Hook {
    const VTABLE: RawWakerVTable =
        RawWakerVTable::new(Self::clone, Self::release, |_| (), Self::release);

    fn waker(on_clone: Option<Box<dyn FnOnce()>>, on_release: Option<Box<dyn FnOnce()>>) -> Waker {
        let hook = Box::into_raw(Box::new(Hook {
            refs: Cell::new(1),
            on_clone: Cell::new(on_clone),
            on_release: Cell::new(on_release),
        }));
        unsafe { Waker::from_raw(RawWaker::new(hook.cast(), &Self::VTABLE)) }
    }

    unsafe fn clone(p: *const ()) -> RawWaker {
        let hook = unsafe { &*p.cast::<Hook>() };
        hook.refs.set(hook.refs.get() + 1);
        if let Some(f) = hook.on_clone.take() {
            f();
        }
        RawWaker::new(p, &Self::VTABLE)
    }

    unsafe fn release(p: *const ()) {
        let hook = unsafe { &*p.cast::<Hook>() };
        hook.refs.set(hook.refs.get() - 1);
        if hook.refs.get() == 0 {
            if let Some(f) = hook.on_release.take() {
                f();
            }
            drop(unsafe { Box::from_raw(p.cast_mut().cast::<Hook>()) });
        }
    }
}

#[test]
fn join_completes_in_waker_callback() {
    // the target task completes while the join handle replaces its waker
    for in_clone in [false, true] {
        let rt = Runtime::new(Box::new(NoopNotify));
        CURRENT_RUNTIME.set(&rt, || {
            let ready = Rc::new(Cell::new(false));
            let waker = Rc::new(RefCell::new(None::<Waker>));
            let (ready2, waker2) = (ready.clone(), waker.clone());
            let mut h = rt.spawn(poll_fn(move |cx| {
                if ready2.get() {
                    Poll::Ready(1)
                } else {
                    *waker2.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                }
            }));
            while rt.poll() == PollResult::PollAgain {}

            let complete: Box<dyn FnOnce()> = Box::new(move || {
                ready.set(true);
                waker.borrow_mut().take().unwrap().wake();
                Runtime::with_current(|rt| while rt.poll() == PollResult::PollAgain {});
            });
            let (on_clone, on_release) = if in_clone {
                (Some(complete), None)
            } else {
                (None, Some(complete))
            };

            let first = Hook::waker(None, on_release);
            let res = Pin::new(&mut h).poll(&mut Context::from_waker(&first));
            assert!(res.is_pending());
            // the join handle keeps the last reference
            drop(first);

            let second = Hook::waker(on_clone, None);
            let res = Pin::new(&mut h).poll(&mut Context::from_waker(&second));
            assert!(matches!(res, Poll::Ready(Ok(1))));
        });
    }
}

#[cfg(ntex_loom)]
#[test]
fn task_loom_reentrant_lifecycle() {
    loom::model(|| {
        join_completes_in_waker_callback();
        reentrant_poll_in_drop();
    });
}

#[cfg(ntex_loom)]
#[test]
fn task_loom_panicking_future_drop() {
    loom::model(panic_in_future_drop);
}

#[test]
fn panic_in_future_drop() {
    struct PanicOnDrop(Counter);

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            self.0.inc();
            panic!("drop panic");
        }
    }

    struct PanicFut {
        _panic: PanicOnDrop,
        _flag: DropFlag,
        ready: bool,
    }

    impl Future for PanicFut {
        type Output = usize;

        fn poll(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<usize> {
            if self.ready { Poll::Ready(1) } else { Poll::Pending }
        }
    }

    let rt = Runtime::new(Box::new(NoopNotify));
    for ready in [true, false] {
        let drops = Counter::default();
        let dropped = Arc::new(AtomicBool::new(false));
        let h = rt.spawn(PanicFut {
            _panic: PanicOnDrop(drops.clone()),
            _flag: DropFlag(dropped.clone()),
            ready,
        });
        let h = if ready {
            h
        } else {
            drain(&rt);
            h.cancel();
            rt.spawn(PanicFut {
                _panic: PanicOnDrop(drops.clone()),
                _flag: DropFlag(dropped.clone()),
                ready: true,
            })
        };
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain(&rt)));
        assert!(res.is_err());
        assert!(dropped.load(Ordering::SeqCst));
        if !ready {
            // the second task panics on the next run
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain(&rt)));
            assert!(res.is_err());
        }
        assert!(h.is_finished());
        assert!(block_on_thread(h).is_err());
        drain(&rt);
        assert_eq!(drops.get(), if ready { 1 } else { 2 });
    }
}

#[test]
fn join_waker_replaced() {
    fn poll_with(h: &mut JoinHandle<usize>, w: &Arc<CountWaker>) -> Poll<usize> {
        let w = Waker::from(w.clone());
        let mut cx = std::task::Context::from_waker(&w);
        Pin::new(h).poll(&mut cx).map(Result::unwrap)
    }

    let rt = Runtime::new(Box::new(NoopNotify));
    for remote in [false, true] {
        let slot = Slot::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let mut h = rt.spawn(wait_polls(slot.clone(), polls.clone(), 2));
        poll_until(&rt, || polls.load(Ordering::SeqCst) == 1);

        let a = Arc::new(CountWaker::default());
        let b = Arc::new(CountWaker::default());
        let (a2, b2) = (a.clone(), b.clone());
        let poll_both = move |mut h| {
            assert!(poll_with(&mut h, &a2).is_pending());
            assert!(poll_with(&mut h, &b2).is_pending());
            h
        };
        h = if remote {
            thread::spawn(move || poll_both(h)).join().unwrap()
        } else {
            poll_both(h)
        };

        slot.lock().unwrap().take().unwrap().wake();
        poll_until(&rt, || b.0.load(Ordering::SeqCst) == 1);
        assert_eq!(a.0.load(Ordering::SeqCst), 0);
        assert_eq!(poll_with(&mut h, &b), Poll::Ready(2));
    }
}
