use std::time::Instant;

use super::*;

fn wait<R>(fut: BlockingResult<R>, timeout: Duration) -> Option<Result<R, BlockingError>> {
    let mut fut = std::pin::pin!(fut);
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let start = Instant::now();
    loop {
        if let Poll::Ready(res) = fut.as_mut().poll(&mut cx) {
            return Some(res);
        }
        if start.elapsed() > timeout {
            return None;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// STATEMENT: Concurrent submissions cannot exceed the blocking pool's thread limit.
///
/// HOW:
/// - Release eight submitters together and count concurrently executing jobs.
/// - Await every result and verify peak concurrency stays within the two-worker limit.
#[test]
fn thread_limit_respected() {
    let pool = ThreadPool::new("test", 2, Duration::from_secs(1));
    let running = Arc::new(AtomicUsize::new(0));
    let max = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(std::sync::Barrier::new(8));

    // concurrent submitters
    let submitters: Vec<_> = (0..8)
        .map(|_| {
            let (pool, running, max, barrier) =
                (pool.clone(), running.clone(), max.clone(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                (0..8)
                    .map(|_| {
                        let (running, max) = (running.clone(), max.clone());
                        pool.execute(move || {
                            let cnt = running.fetch_add(1, Ordering::SeqCst) + 1;
                            max.fetch_max(cnt, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(5));
                            running.fetch_sub(1, Ordering::SeqCst);
                        })
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for s in submitters {
        for res in s.join().unwrap() {
            assert_eq!(wait(res, Duration::from_secs(10)), Some(Ok(())));
        }
    }
    assert!(max.load(Ordering::SeqCst) <= 2, "{max:?} tasks ran at once");
}

/// STATEMENT: Work submitted around an idle worker's timeout is not stranded.
///
/// HOW:
/// - Repeatedly submit near the idle timeout of a single-worker pool.
/// - Require each result to arrive within a bounded wait.
#[test]
fn idle_workers_do_not_strand_tasks() {
    let pool = ThreadPool::new("test", 1, Duration::from_millis(2));
    for i in 0..300u64 {
        // submit around the moment the idle worker times out
        thread::sleep(Duration::from_micros(1500 + (i % 10) * 100));
        let res = wait(pool.execute(move || i), Duration::from_secs(5));
        assert_eq!(res, Some(Ok(i)), "task {i} was not executed");
    }
}

/// STATEMENT: Blocking work runs inline without a system and converts panics to errors.
///
/// HOW:
/// - Spawn blocking work on a plain thread and compare its execution thread.
/// - Panic in another job and verify the returned error and its display text.
#[test]
fn spawn_blocking_without_system() {
    thread::spawn(|| {
        let tid = thread::current().id();
        let res = spawn_blocking(move || thread::current().id() == tid);
        assert_eq!(wait(res, Duration::from_secs(1)), Some(Ok(true)));

        let res = spawn_blocking(|| panic!("blocking"));
        assert_eq!(
            wait(res, Duration::from_secs(1)),
            Some(Err::<(), _>(BlockingError))
        );
    })
    .join()
    .unwrap();
    assert_eq!(
        BlockingError.to_string(),
        "Blocking task failed or was canceled"
    );
}

/// STATEMENT: Detaching a blocking result lets its job complete.
///
/// HOW:
/// - Detach a job in a running system and await its independent completion signal.
#[test]
fn detached_blocking_task_runs() {
    crate::System::new("test", crate::testing::TestRunner).block_on(async {
        let (tx, rx) = oneshot::async_channel();
        spawn_blocking(move || tx.send(1).unwrap()).detach();
        assert_eq!(rx.await, Ok(1));
    });
}

/// STATEMENT: Detachment works without a runtime and differs from cancelling a queued result.
///
/// HOW:
/// - Detach inline work, then queue dropped and detached jobs behind a blocked worker.
/// - Release the worker and confirm only the detached queued job runs.
#[test]
fn detach_without_runtime() {
    thread::spawn(|| spawn_blocking(|| 1).detach())
        .join()
        .unwrap();

    // occupy the only worker, the next tasks wait in the queue
    let pool = ThreadPool::new("test", 1, Duration::from_secs(1));
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let first = pool.execute(move || {
        start_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    start_rx.recv().unwrap();

    let dropped = Arc::new(AtomicBool::new(false));
    let dropped2 = dropped.clone();
    drop(pool.execute(move || dropped2.store(true, Ordering::Relaxed)));
    let (tx, rx) = std::sync::mpsc::channel();
    pool.execute(move || tx.send(1).unwrap()).detach();
    release_tx.send(()).unwrap();

    assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(1));
    assert_eq!(wait(first, Duration::from_secs(5)), Some(Ok(())));
    // dropped results cancel queued work
    assert!(!dropped.load(Ordering::Relaxed));
}

/// STATEMENT: Pool metrics account for queued, active, completed, waiting, and busy work.
///
/// HOW:
/// - Hold one job active while another waits and inspect their counts.
/// - Release both jobs and verify final counts and accumulated durations.
#[test]
fn metrics() {
    let pool = ThreadPool::new("test", 1, Duration::from_secs(1));
    let m = pool.metrics();
    assert_eq!((m.thread_limit, m.threads, m.tasks), (1, 0, 0));

    // occupy the only worker, the second task waits in the queue
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let first = pool.execute(move || {
        start_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    start_rx.recv().unwrap();
    let second = pool.execute(|| ());

    let m = pool.metrics();
    assert_eq!((m.threads, m.active_tasks, m.queue_depth), (1, 1, 1));
    assert_eq!(m.tasks, 1);

    let sleep = Duration::from_millis(20);
    thread::sleep(sleep);
    release_tx.send(()).unwrap();
    assert_eq!(wait(first, Duration::from_secs(5)), Some(Ok(())));
    assert_eq!(wait(second, Duration::from_secs(5)), Some(Ok(())));

    let m = pool.metrics();
    assert_eq!((m.active_tasks, m.queue_depth, m.tasks), (0, 0, 2));
    assert!(m.wait_duration >= sleep);
    assert!(m.busy_duration >= sleep);
}

/// STATEMENT: Dropping a pool drains accepted work and releases workers without waiting for idle timeout.
///
/// HOW:
/// - Drop a pool with one blocked job and one queued job, then release the first.
/// - Verify both results and wait for the worker count to reach zero.
#[test]
fn dropped_pool_releases_workers() {
    // the idle timeout is never reached, workers exit on disconnection
    let pool = ThreadPool::new("test", 1, Duration::from_secs(60));
    let counter = pool.counter.clone();
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let first = pool.execute(move || {
        start_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    start_rx.recv().unwrap();
    let queued = pool.execute(|| 1);
    drop(pool);
    release_tx.send(()).unwrap();

    assert_eq!(wait(first, Duration::from_secs(5)), Some(Ok(())));
    assert_eq!(wait(queued, Duration::from_secs(5)), Some(Ok(1)));
    let start = Instant::now();
    while counter.load(Ordering::Acquire) != 0 {
        assert!(start.elapsed() < Duration::from_secs(5), "worker is alive");
        thread::sleep(Duration::from_millis(1));
    }
}

/// STATEMENT: A panic dropping an unclaimed result does not kill the pool worker.
///
/// HOW:
/// - Abandon a running job whose result destructor panics and queue another job.
/// - Release the first job and verify the second completes on the surviving worker.
#[test]
fn result_drop_panic_keeps_worker() {
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("result destructor");
        }
    }

    let pool = ThreadPool::new("test", 1, Duration::from_secs(60));
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let first = pool.execute(move || {
        start_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        PanicOnDrop
    });
    start_rx.recv().unwrap();
    let queued = pool.execute(|| 1);
    // the unclaimed result is dropped on the worker
    drop(first);
    release_tx.send(()).unwrap();

    assert_eq!(wait(queued, Duration::from_secs(2)), Some(Ok(1)));
    assert_eq!(pool.metrics().threads, 1);
}

/// STATEMENT: A configured zero thread limit still permits blocking work to complete.
///
/// HOW:
/// - Submit to a zero-limit pool and require its result within a bounded wait.
#[test]
fn zero_thread_limit() {
    let pool = ThreadPool::new("test", 0, Duration::from_secs(1));
    assert_eq!(
        wait(pool.execute(|| 1), Duration::from_secs(5)),
        Some(Ok(1))
    );
}
