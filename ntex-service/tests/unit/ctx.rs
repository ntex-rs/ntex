use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::{cell::Cell, cell::RefCell, future::poll_fn, task::Wake};

use ntex::channel::{condition, oneshot};
use ntex::{rt::spawn, time, util::lazy, util::select};

use super::*;
use crate::{BindCtx, Pipeline, ReadyNotifier};

struct Srv(Rc<Cell<usize>>, condition::Waiter, ReadyNotifier);

impl Service<(), &'static str> for Srv {
    type Res = &'static str;
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.2);
    }

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        self.0.set(self.0.get() + 1);
        self.1.ready().await;
        Ok(())
    }

    async fn call(&self, req: &'static str, ctx: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
        let _ = format!("{ctx:?}");
        let _ = format!("{:?}", ctx.id());
        let () = *ctx;
        #[allow(clippy::clone_on_copy)]
        let _ = ctx.clone();
        Ok(req)
    }
}

/// STATEMENT: Repeated readiness polls share a check and notifications invalidate its cached success.
///
/// HOW:
/// - Poll a condition-gated service repeatedly before and after releasing it.
/// - Notify readiness changes and verify exactly one additional check starts each time.
#[ntex::test]
async fn test_ready() {
    let cnt = Rc::new(Cell::new(0));
    let con = condition::Condition::new();

    let changed = ReadyNotifier::new();
    let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), changed.clone()));
    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Pending);
    assert_eq!(cnt.get(), 1);

    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Pending);
    assert_eq!(cnt.get(), 1);

    con.notify(());
    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Ready(Ok(())));
    assert_eq!(cnt.get(), 1);

    assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
    assert_eq!(cnt.get(), 1);
    changed.notify();
    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Pending);
    assert_eq!(cnt.get(), 2);

    con.notify(());
    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Ready(Ok(())));
    assert_eq!(cnt.get(), 2);

    changed.notify();
    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Pending);
    assert_eq!(cnt.get(), 3);
}

struct WakeCounter {
    clones: Cell<usize>,
    wakes: Cell<usize>,
}

fn counting_waker() -> (&'static WakeCounter, Waker) {
    use std::task::{RawWaker, RawWakerVTable};

    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake, drop);

    unsafe fn clone(data: *const ()) -> RawWaker {
        let cnt = unsafe { &*data.cast::<WakeCounter>() };
        cnt.clones.set(cnt.clones.get() + 1);
        RawWaker::new(data, &VTABLE)
    }
    unsafe fn wake(data: *const ()) {
        let cnt = unsafe { &*data.cast::<WakeCounter>() };
        cnt.wakes.set(cnt.wakes.get() + 1);
    }
    unsafe fn drop(_: *const ()) {}

    let cnt: &'static WakeCounter = Box::leak(Box::new(WakeCounter {
        clones: Cell::new(0),
        wakes: Cell::new(0),
    }));
    let raw = RawWaker::new(std::ptr::from_ref(cnt).cast(), &VTABLE);
    (cnt, unsafe { Waker::from_raw(raw) })
}

/// STATEMENT: Readiness waiter notification reuses allocated waker buffers.
///
/// HOW:
/// - Register three waiters and repeatedly complete their shared readiness owner.
/// - Check wake counts, empty active storage, and stable buffer pointers and capacity.
#[test]
fn notify_reuses_waiter_buffers() {
    let waiters = WaitersRef::new();
    let idx = [waiters.insert(), waiters.insert(), waiters.insert()];
    let (cnt, waker) = counting_waker();
    let mut cx = Context::from_waker(&waker);
    let mut buffers = None;
    for round in 1..=3 {
        assert!(
            waiters
                .run(idx[0], &mut cx, |_| Poll::<()>::Pending)
                .is_pending()
        );
        for i in &idx[1..] {
            assert!(waiters.run(*i, &mut cx, |_| Poll::Ready(())).is_pending());
        }
        assert!(waiters.run(idx[0], &mut cx, |_| Poll::Ready(())).is_ready());
        assert_eq!(cnt.wakes.get(), round * 3);
        assert_eq!(waiters.get_wakers().as_slice(), [0u32; 0]);

        let current = (
            waiters.get_wakers().as_ptr(),
            waiters.scratch.borrow().as_ptr(),
        );
        assert!(waiters.scratch.borrow().capacity() >= 3);
        assert_eq!(*buffers.get_or_insert(current), current);
    }
}

/// Wakes the dispatcher's task context during the readiness check
struct WakeSrv;

impl Service<(), &'static str> for WakeSrv {
    type Res = &'static str;
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        std::future::poll_fn(|cx| {
            cx.waker().wake_by_ref();
            Poll::Ready(Ok(()))
        })
        .await
    }

    async fn call(&self, req: &'static str, _: Ctx<'_, Self>) -> Result<&'static str, ()> {
        Ok(req)
    }
}

struct Nested<S>(S);

impl<S: Service<(), &'static str>> Service<(), &'static str> for Nested<S> {
    type Res = S::Res;
    type Error = S::Error;

    crate::forward_bind!((), 0);

    async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), Self::Error> {
        ctx.ready(&self.0).await
    }

    async fn call(&self, req: &'static str, ctx: Ctx<'_, Self>) -> Result<Self::Res, Self::Error> {
        ctx.call(&self.0, req).await
    }
}

/// STATEMENT: Cached readiness avoids recloning an unchanged waker but replaces a different one.
///
/// HOW:
/// - Poll nested forwarding services with a counting waker, then repeat cached polls.
/// - Verify unchanged clone/wake counts and one clone when a new waker is supplied.
#[ntex::test]
async fn test_ready_waker_not_recloned() {
    let srv = Pipeline::new((), Nested(Nested(WakeSrv)));

    let (cnt1, waker1) = counting_waker();
    let mut cx = Context::from_waker(&waker1);
    assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));
    let clones = cnt1.clones.get();
    let wakes = cnt1.wakes.get();
    for _ in 0..4 {
        assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));
    }
    assert_eq!(cnt1.clones.get(), clones);
    assert_eq!(cnt1.wakes.get(), wakes);

    // a different waker replaces the stored one
    let (cnt2, waker2) = counting_waker();
    let mut cx = Context::from_waker(&waker2);
    assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(cnt2.clones.get(), 1);
    assert_eq!(cnt2.wakes.get(), 0);
    assert_eq!(cnt1.wakes.get(), wakes);
}

/// STATEMENT: Dropping the current readiness future releases ownership for another checker.
///
/// HOW:
/// - Start a bound readiness wait, then cancel it while a pipeline poll is waiting.
/// - Release the service condition and verify the pipeline readiness poll completes.
#[ntex::test]
async fn test_ready_on_drop() {
    let cnt = Rc::new(Cell::new(0));
    let con = condition::Condition::new();
    let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));

    let srv1 = srv.bind();
    let (tx, rx) = oneshot::channel();
    spawn(async move {
        select(rx, srv1.ready()).await;
        time::sleep(time::Millis(25000)).await;
    });
    time::sleep(time::Millis(250)).await;

    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Pending);

    let _ = tx.send(());
    time::sleep(time::Millis(250)).await;

    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Pending);

    con.notify(());
    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Ready(Ok(())));
}

struct PanicOnce(Cell<bool>, ReadyNotifier);

impl Service<(), &'static str> for PanicOnce {
    type Res = &'static str;
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.1);
    }

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), Self::Error> {
        assert!(self.0.replace(true), "ready panic");
        Ok(())
    }

    async fn call(&self, req: &'static str, _: Ctx<'_, Self>) -> Result<&'static str, ()> {
        Ok(req)
    }
}

/// STATEMENT: A readiness panic does not retain ownership or prevent later calls.
///
/// HOW:
/// - Catch the first readiness panic and retry successfully.
/// - Invalidate between checks from different bindings and verify a subsequent call succeeds.
#[ntex::test]
async fn test_ready_after_panic() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let changed = ReadyNotifier::new();
    let srv = Pipeline::new((), PanicOnce(Cell::new(false), changed.clone()));
    let b = srv.bind();

    let mut cx = Context::from_waker(Waker::noop());
    assert!(catch_unwind(AssertUnwindSafe(|| srv.poll_ready(&mut cx))).is_err());
    assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));

    // the panicked check must not keep ownership of later checks
    changed.notify();
    let mut fut = Box::pin(b.ready());
    assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    changed.notify();
    let mut fut = Box::pin(srv.ready());
    assert_eq!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(b.call("ok").await, Ok("ok"));
}

/// STATEMENT: A shutting-down pipeline reports readiness without restarting a pending service check.
///
/// HOW:
/// - Begin pending readiness, cancel a competing wait, and shut down the pipeline.
/// - Verify readiness polls complete before and after the original condition is notified.
#[ntex::test]
async fn test_ready_after_shutdown() {
    let cnt = Rc::new(Cell::new(0));
    let con = condition::Condition::new();
    let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));

    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Pending);

    let (tx, rx) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();
    spawn(async move {
        select(rx, srv.ready()).await;
        srv.shutdown().await;
        let _ = tx2.send(srv);
    });
    time::sleep(time::Millis(250)).await;

    let _ = tx.send(());
    let srv = rx2.await.unwrap();

    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Ready(Ok(())));

    con.notify(());
    let res = lazy(|cx| srv.poll_ready(cx)).await;
    assert_eq!(res, Poll::Ready(Ok(())));
}

/// STATEMENT: Pipeline polling can complete readiness after poll-based shutdown.
///
/// HOW:
/// - Construct a service whose readiness condition has not been released.
/// - Complete poll_shutdown and verify poll_ready also completes.
#[ntex::test]
async fn test_pipeline_binding_after_shutdown() {
    let cnt = Rc::new(Cell::new(0));
    let con = condition::Condition::new();
    let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));
    poll_fn(|cx| srv.poll_shutdown(cx)).await;
    let _ = poll_fn(|cx| srv.poll_ready(cx)).await;
}

/// STATEMENT: Concurrent bound calls share one successful readiness check.
///
/// HOW:
/// - Start an owned call after an explicit ready wait and a second borrowed call.
/// - Release their shared condition and verify both responses and one readiness invocation.
#[ntex::test]
async fn test_shared_call() {
    let data = Rc::new(RefCell::new(Vec::new()));

    let cnt = Rc::new(Cell::new(0));
    let con = condition::Condition::new();

    let srv = Pipeline::new((), Srv(cnt.clone(), con.wait(), ReadyNotifier::new()));

    let srv1 = srv.bind();
    let data1 = data.clone();
    ntex::rt::spawn(async move {
        let _ = srv1.ready().await;
        let fut = srv1.call_static("srv1");
        assert!(format!("{fut:?}").contains("PipelineCall"));
        let i = fut.await.unwrap();
        data1.borrow_mut().push(i);
    });

    let srv2 = srv.bind();
    let data2 = data.clone();
    ntex::rt::spawn(async move {
        let i = srv2.call("srv2").await.unwrap();
        data2.borrow_mut().push(i);
    });
    time::sleep(time::Millis(50)).await;

    con.notify(());
    time::sleep(time::Millis(150)).await;

    assert_eq!(cnt.get(), 1);
    assert_eq!(&*data.borrow(), &["srv1", "srv2"]);
}

/// Readiness with a single waker slot, a later check replaces the earlier waker.
#[derive(Clone)]
struct SingleWaker(Rc<(Cell<bool>, RefCell<Option<Waker>>, ReadyNotifier)>);

impl SingleWaker {
    fn new(open: bool) -> Self {
        Self(Rc::new((
            Cell::new(open),
            RefCell::new(None),
            ReadyNotifier::new(),
        )))
    }

    fn close(&self) {
        self.0.0.set(false);
        self.0.2.notify();
    }

    fn open(&self, wake: bool) {
        self.0.0.set(true);
        if wake && let Some(waker) = self.0.1.borrow_mut().take() {
            waker.wake();
        }
    }
}

impl Service<(), ()> for SingleWaker {
    type Res = ();
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.0.2);
    }

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
        poll_fn(|cx| {
            if self.0.0.get() {
                Poll::Ready(Ok(()))
            } else {
                *self.0.1.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }

    async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), ()> {
        Ok(())
    }
}

/// Takes a slot of a limit, pauses, then calls the inner service.
struct Limited {
    inner: SingleWaker,
    cap: usize,
    used: Cell<usize>,
    waiters: RefCell<Vec<Waker>>,
    changed: ReadyNotifier,
    pause: condition::Condition,
}

impl Limited {
    fn new(inner: SingleWaker, cap: usize, pause: &condition::Condition) -> Self {
        Self {
            inner,
            cap,
            used: Cell::new(0),
            waiters: RefCell::default(),
            changed: ReadyNotifier::new(),
            pause: pause.clone(),
        }
    }
}

impl Service<(), ()> for Limited {
    type Res = ();
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.changed);
        ctx.bind::<_, (), ()>(&self.inner);
    }

    async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), ()> {
        poll_fn(|cx| {
            if self.used.get() < self.cap {
                Poll::Ready(())
            } else {
                self.waiters.borrow_mut().push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
        ctx.ready(&self.inner).await
    }

    async fn call(&self, (): (), ctx: Ctx<'_, Self>) -> Result<(), ()> {
        self.used.set(self.used.get() + 1);
        if self.used.get() == self.cap {
            self.changed.notify();
        }
        self.pause.wait().ready().await;
        let result = ctx.call(&self.inner, ()).await;
        self.used.set(self.used.get() - 1);
        for waker in self.waiters.borrow_mut().drain(..) {
            waker.wake();
        }
        result
    }
}

fn spawn_ready(pl: &Pipeline<(), (), ()>, done: &Rc<Cell<usize>>) {
    let (pl, done) = (pl.bind(), done.clone());
    spawn(async move {
        pl.ready().await.unwrap();
        done.set(done.get() + 1);
    });
}

fn spawn_call(pl: &Pipeline<(), (), ()>, done: &Rc<Cell<usize>>) -> oneshot::Sender<()> {
    let (tx, rx) = oneshot::channel();
    let (pl, done) = (pl.bind(), done.clone());
    spawn(async move {
        if let ntex::util::Either::Right(result) = select(rx, pl.call(())).await {
            result.unwrap();
            done.set(done.get() + 1);
        }
    });
    tx
}

async fn settle() {
    time::sleep(time::Millis(10)).await;
}

/// STATEMENT: Call-time inner readiness must not deadlock behind an owner waiting for that call.
///
/// HOW:
/// - Pause a call while it holds the only capacity slot and start a readiness owner.
/// - Resume the call and verify both the call and owner finish.
#[ntex::test]
async fn call_time_check_does_not_wait_for_owner_waiting_on_call() {
    let inner = SingleWaker::new(true);
    let pause = condition::Condition::new();
    let pl = Pipeline::new((), Limited::new(inner, 1, &pause));
    let (calls, checks) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));

    let _call = spawn_call(&pl, &calls);
    settle().await;
    // Owns the readiness gate and waits for the slot held by the call.
    spawn_ready(&pl, &checks);
    settle().await;
    assert_eq!((calls.get(), checks.get()), (0, 0));

    pause.notify(());
    settle().await;
    assert_eq!((calls.get(), checks.get()), (1, 1));
}

/// STATEMENT: Competing call-time checks wake each other and the readiness owner.
///
/// HOW:
/// - Pause two calls, close a single-waker inner service, and start an owner check.
/// - Resume both calls, wake only the last stored waker, and verify all three operations finish.
#[ntex::test]
async fn call_time_checks_wake_owner_and_each_other() {
    let inner = SingleWaker::new(true);
    let pause = condition::Condition::new();
    let pl = Pipeline::new((), Limited::new(inner.clone(), 10, &pause));
    let (calls, checks) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));

    let _call1 = spawn_call(&pl, &calls);
    let _call2 = spawn_call(&pl, &calls);
    settle().await;
    inner.close();
    // Owns the readiness gate and waits for the inner service.
    spawn_ready(&pl, &checks);
    settle().await;

    // Both calls bypass the owner and replace the inner waker.
    pause.notify(());
    settle().await;
    assert_eq!((calls.get(), checks.get()), (0, 0));

    // Only the last stored waker is woken.
    inner.open(true);
    settle().await;
    assert_eq!((calls.get(), checks.get()), (2, 1));
}

/// STATEMENT: Cancelling a call-time readiness check wakes an owner whose waker was replaced.
///
/// HOW:
/// - Park a call and an owner on a closed single-waker service.
/// - Open it without waking, cancel the call, and verify only the owner completes.
#[ntex::test]
async fn dropped_call_time_check_wakes_owner() {
    let inner = SingleWaker::new(true);
    let pause = condition::Condition::new();
    let pl = Pipeline::new((), Limited::new(inner.clone(), 10, &pause));
    let (calls, checks) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));

    let call = spawn_call(&pl, &calls);
    settle().await;
    inner.close();
    spawn_ready(&pl, &checks);
    settle().await;
    pause.notify(());
    settle().await;

    inner.open(false);
    let _ = call.send(());
    settle().await;
    assert_eq!((calls.get(), checks.get()), (0, 1));
}

/// STATEMENT: Call-time readiness checks can take a free gate and make progress together.
///
/// HOW:
/// - Pause two calls, invalidate their inner service, and resume them without an owner check.
/// - Reopen and wake the inner service, verifying both calls complete.
#[ntex::test]
async fn call_time_checks_take_free_gate() {
    let inner = SingleWaker::new(true);
    let pause = condition::Condition::new();
    let pl = Pipeline::new((), Limited::new(inner.clone(), 10, &pause));
    let calls = Rc::new(Cell::new(0));

    let _call1 = spawn_call(&pl, &calls);
    let _call2 = spawn_call(&pl, &calls);
    settle().await;
    inner.close();
    pause.notify(());
    settle().await;
    assert_eq!(calls.get(), 0);

    inner.open(true);
    settle().await;
    assert_eq!(calls.get(), 2);
}

/// Runs `0` after `rx` resolves; readiness also depends on `1`.
struct Delayed(
    SingleWaker,
    SingleWaker,
    RefCell<Option<oneshot::Receiver<()>>>,
);

impl Service<(), ()> for Delayed {
    type Res = ();
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind(&self.0);
        ctx.bind(&self.1);
    }

    async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), ()> {
        ctx.ready(&self.1).await?;
        ctx.ready(&self.0).await
    }

    async fn call(&self, (): (), ctx: Ctx<'_, Self>) -> Result<(), ()> {
        let rx = self.2.borrow_mut().take().unwrap();
        let _ = rx.await;
        ctx.call(&self.0, ()).await
    }
}

thread_local! {
    static WAKE_NOTIFIER: RefCell<Option<ReadyNotifier>> = const { RefCell::new(None) };
}

/// Invalidates readiness on each wake, except notification feedback.
struct NotifyingWaker(AtomicUsize);

impl Wake for NotifyingWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        assert!(
            self.0.fetch_add(1, Ordering::Relaxed) < 100,
            "readiness spins"
        );
        if let Some(source) = WAKE_NOTIFIER.with_borrow_mut(Option::take) {
            source.notify();
            WAKE_NOTIFIER.with_borrow_mut(|slot| *slot = Some(source));
        }
    }
}

/// STATEMENT: A call-time readiness check yields when waking the owner repeatedly invalidates it.
///
/// HOW:
/// - Install an owner waker that invalidates the delayed call's nested readiness on each wake.
/// - Suppress notification feedback in the waker to avoid same-source reentry.
/// - Resume the call and verify its poll returns Pending with a bounded wake count.
#[ntex::test]
async fn call_check_yields_when_waking_owner_invalidates_it() {
    let inner = SingleWaker::new(true);
    let gate = SingleWaker::new(true);
    let (inner_h, gate_h) = (SingleWaker(inner.0.clone()), SingleWaker(gate.0.clone()));
    let (tx, rx) = oneshot::channel();
    let pl = Pipeline::new((), Delayed(inner, gate, RefCell::new(Some(rx))));
    let mut noop = Context::from_waker(Waker::noop());

    let caller = pl.bind();
    let mut call = Box::pin(caller.call(()));
    assert!(call.as_mut().poll(&mut noop).is_pending());

    // The owner's waker invalidates the call's nested check on every wake.
    gate_h.close();
    WAKE_NOTIFIER.with(|n| *n.borrow_mut() = Some(inner_h.0.2.clone()));
    let owner = pl.bind();
    let mut ready = Box::pin(owner.ready());
    let wakes = Arc::new(NotifyingWaker(AtomicUsize::new(0)));
    let waker = Waker::from(wakes.clone());
    assert!(
        ready
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );

    let _ = tx.send(());
    assert!(call.as_mut().poll(&mut noop).is_pending());
    assert!((2..10).contains(&wakes.0.load(Ordering::Relaxed)));
}
