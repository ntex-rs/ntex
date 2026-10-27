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

/// STATEMENT: Remote completion drops the pinned future before waking the joiner, even if drop panics.
///
/// HOW:
/// - Complete pinned futures with normal and panicking destructors.
/// - Verify stable addresses, one drop and wake, and exactly one retrievable output.
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

/// STATEMENT: Remote cancellation and runner drop destroy pinned futures in place before notification.
///
/// HOW:
/// - Cancel or drop pending runners with normal and panicking future destructors.
/// - Verify one owner-side destruction, one wake, no output, and idempotent later cleanup.
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
