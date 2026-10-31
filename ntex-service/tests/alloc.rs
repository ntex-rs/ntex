//! Readiness allocation counts, excluding executor and service-owned allocations.
//!
//! Run the timing comparison separately from other tests:
//! `cargo test -p ntex-service --release --test alloc benchmark_readiness -- --ignored --exact --nocapture`.
//! It reports seven warmed samples using the counting system allocator, with no
//! executor or I/O. Timings are descriptive, not pass/fail thresholds. Cached
//! calls still include dispatch boxing; invalidated cases include notification.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    future::{Future, poll_fn},
    pin::pin,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use ntex_service::{
    BindCtx, Ctx, Pipeline, ReadyCallback, ReadyNotifier, Service, ServiceBinding, ServiceCaller,
    boxed, map_state, pipeline::PipelineState, ready,
};

struct Counting;

#[derive(Copy, Clone, Debug, Default)]
struct Counts {
    allocs: usize,
    bytes: usize,
    deallocs: usize,
}

thread_local! {
    static COUNTS: Cell<Counts> = const {
        Cell::new(Counts { allocs: 0, bytes: 0, deallocs: 0 })
    };
}

fn record_alloc(bytes: usize) {
    COUNTS.with(|counts| {
        let mut value = counts.get();
        value.allocs += 1;
        value.bytes += bytes;
        counts.set(value);
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_alloc(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_alloc(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        COUNTS.with(|counts| {
            let mut value = counts.get();
            value.deallocs += 1;
            counts.set(value);
        });
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_alloc(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn measure<R>(f: impl FnOnce() -> R) -> (R, Counts) {
    let before = COUNTS.with(Cell::get);
    let result = f();
    let after = COUNTS.with(Cell::get);
    (
        result,
        Counts {
            allocs: after.allocs - before.allocs,
            bytes: after.bytes - before.bytes,
            deallocs: after.deallocs - before.deallocs,
        },
    )
}

fn expect_allocs<R>(name: &str, expected: usize, f: impl FnOnce() -> R) -> R {
    let (result, counts) = measure(f);
    assert_eq!(counts.allocs, expected, "{name}: {counts:?}");
    result
}

fn complete<F: Future>(future: F) -> F::Output {
    let Poll::Ready(result) = pin!(future).poll(&mut Context::from_waker(Waker::noop())) else {
        panic!("expected an immediately ready future");
    };
    result
}

#[derive(Default)]
struct Gate {
    changed: ReadyNotifier,
    available: Cell<bool>,
    checks: Cell<usize>,
    active: Cell<usize>,
    notifications: Cell<usize>,
}

struct Probe(Rc<Gate>, ReadyCallback);

struct ActiveCheck<'a>(&'a Gate);

impl Drop for ActiveCheck<'_> {
    fn drop(&mut self) {
        self.0.active.set(self.0.active.get() - 1);
    }
}

impl Service<usize, usize> for Probe {
    type Res = usize;
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.0.changed);
        ctx.on_notify(&self.1);
    }

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), ()> {
        self.0.checks.set(self.0.checks.get() + 1);
        assert_eq!(self.0.active.replace(1), 0, "duplicate checker");
        let _guard = ActiveCheck(&self.0);
        poll_fn(|_| {
            if self.0.available.get() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, ()> {
        Ok(req + *ctx.st())
    }
}

fn probe() -> (Probe, Rc<Gate>) {
    let gate = Rc::new(Gate::default());
    gate.available.set(true);
    let captured = gate.clone();
    let callback = ReadyCallback::new(move || {
        captured.notifications.set(captured.notifications.get() + 1);
    });
    (Probe(gate.clone(), callback), gate)
}

/// STATEMENT: Constructing an unbound service binding does not allocate readiness storage.
///
/// HOW:
/// - Create the service before measurement and wrap it without binding it to a pipeline.
/// - Require zero allocations for the wrapper and no readiness checks.
#[test]
fn unbound_service_binding_does_not_allocate() {
    let (svc, gate) = probe();
    let binding = expect_allocs("unbound service binding", 0, || ServiceBinding::new(svc));
    assert_eq!(gate.checks.get(), 0);
    drop(binding);
}

/// STATEMENT: Registered notification callbacks add no delivery allocations.
///
/// HOW:
/// - Measure notification without extra callbacks, then register 64 counted callbacks.
/// - Compare allocation, byte, and deallocation counts over two deliveries and verify every callback runs.
#[test]
fn notification_delivery_reuses_registered_callback_storage() {
    let (svc, gate) = probe();
    let pl = Pipeline::new(10, svc);
    let (_, baseline) = measure(|| gate.changed.notify());
    let calls = Rc::new(Cell::new(0));
    for _ in 0..64 {
        let captured = calls.clone();
        pl.on_notify(&ReadyCallback::new(move || {
            captured.set(captured.get() + 1);
        }));
    }
    for expected in [64, 128] {
        let (_, callbacks) = measure(|| gate.changed.notify());
        assert_eq!(
            (callbacks.allocs, callbacks.bytes, callbacks.deallocs),
            (baseline.allocs, baseline.bytes, baseline.deallocs),
            "registered callbacks must not add notification allocations"
        );
        assert_eq!(calls.get(), expected);
    }
}

/// STATEMENT: Invalidating a single ready scope does not allocate.
///
/// HOW:
/// - Warm a pipeline's readiness cache and measure a source notification.
/// - Verify zero allocations, invalidated readiness, and one callback invocation.
#[test]
fn single_scope_notification_does_not_allocate() {
    let (svc, gate) = probe();
    let pl = Pipeline::new(10, svc);
    complete(pl.ready()).unwrap();
    expect_allocs("notify", 0, || gate.changed.notify());
    assert!(!pl.is_ready());
    assert_eq!(gate.notifications.get(), 1);
}

/// STATEMENT: Scoped dependency notifications allocate no traversal storage, including on their first delivery.
///
/// HOW:
/// - Share one notifier across one, two, eight, or 64 pipelines with scoped child services.
/// - Refresh readiness before each notification without warming notification storage.
/// - Require zero allocations while every pipeline becomes invalid and every child callback runs.
#[test]
fn graph_notifications_do_not_allocate() {
    for parents in [1, 2, 8, 64] {
        let (svc, gate) = probe();
        let pipelines: Vec<_> = (0..parents)
            .map(|_| {
                Pipeline::new(
                    10,
                    Forward(ServiceBinding::new(Probe(svc.0.clone(), svc.1.clone()))),
                )
            })
            .collect();
        for expected in [parents, parents * 2] {
            for pipeline in &pipelines {
                complete(pipeline.ready()).unwrap();
            }
            expect_allocs("notify dependency graph", 0, || gate.changed.notify());
            assert!(pipelines.iter().all(|pipeline| !pipeline.is_ready()));
            assert_eq!(gate.notifications.get(), expected);
        }
    }
}

struct Parked(Rc<RefCell<Option<Waker>>>);

impl Service<(), ()> for Parked {
    type Res = ();
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
        poll_fn(|cx| {
            *self.0.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        })
        .await
    }

    async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), ()> {
        Ok(())
    }
}

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// STATEMENT: Notifying a single pending caller allocates nothing for either retained or borrowed checks.
///
/// HOW:
/// - Park each checker form on a closed gate and notify before and after making it available.
/// - Verify zero notification allocations, expected wakes, and eventual checker completion.
#[test]
fn notification_of_single_pending_caller_does_not_allocate() {
    for retained in [false, true] {
        let (svc, gate) = probe();
        gate.available.set(false);
        let pl = Pipeline::new(10, svc);
        let count = Arc::new(WakeCount::default());
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let mut caller = pin!(pl.ready());

        let first_wakes = if retained { 1 } else { 2 };
        for (available, expected_wakes) in [(false, first_wakes), (true, 1)] {
            let result = if retained {
                pl.poll_ready(&mut cx)
            } else {
                caller.as_mut().poll(&mut cx)
            };
            assert!(result.is_pending());
            assert_eq!(gate.active.get(), 1);
            let wakes = count.0.load(Ordering::Relaxed);
            gate.available.set(available);
            expect_allocs("notify pending caller", 0, || gate.changed.notify());
            assert_eq!(count.0.load(Ordering::Relaxed), wakes + expected_wakes);
        }

        let result = if retained {
            pl.poll_ready(&mut cx)
        } else {
            caller.as_mut().poll(&mut cx)
        };
        assert_eq!(result, Poll::Ready(Ok(())));
        assert_eq!(gate.active.get(), 0);
    }
}

/// STATEMENT: Notification wakes many pending observers without allocating a wake buffer.
///
/// HOW:
/// - Park a borrowed or retained checker and eight additional callers on one pipeline.
/// - Measure the first notification and require zero allocations and a wake for every observer.
/// - Open the gate and verify all callers complete with no active check remaining.
#[test]
fn notification_of_many_pending_callers_does_not_allocate() {
    for retained in [false, true] {
        let (svc, gate) = probe();
        gate.available.set(false);
        let pl = Pipeline::new(10, svc);
        let bindings: Vec<_> = (0..8).map(|_| pl.bind()).collect();
        let count = Arc::new(WakeCount::default());
        let waker = Waker::from(count.clone());
        let mut cx = Context::from_waker(&waker);
        let mut owner = pin!(pl.ready());
        let result = if retained {
            pl.poll_ready(&mut cx)
        } else {
            owner.as_mut().poll(&mut cx)
        };
        assert!(result.is_pending());
        let mut waiters: Vec<_> = bindings.iter().map(|b| Box::pin(b.ready())).collect();
        for waiter in &mut waiters {
            assert!(waiter.as_mut().poll(&mut cx).is_pending());
        }
        let before = count.0.load(Ordering::Relaxed);
        expect_allocs("notify many pending callers", 0, || gate.changed.notify());
        assert_eq!(
            count.0.load(Ordering::Relaxed),
            before + if retained { 9 } else { 10 }
        );
        gate.available.set(true);
        gate.changed.notify();
        let result = if retained {
            pl.poll_ready(&mut cx)
        } else {
            owner.as_mut().poll(&mut cx)
        };
        assert_eq!(result, Poll::Ready(Ok(())));
        for waiter in &mut waiters {
            assert_eq!(waiter.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        }
        assert_eq!(gate.active.get(), 0);
    }
}

/// STATEMENT: Waking a checker with one observer requires no allocation.
///
/// HOW:
/// - Park retained readiness and capture the service's checker waker.
/// - Re-register and wake it twice, measuring zero allocations and one task wake per iteration.
#[test]
fn checker_wake_of_single_observer_does_not_allocate() {
    let parked = Rc::new(RefCell::new(None));
    let pl = Pipeline::new((), Parked(parked.clone()));
    let count = Arc::new(WakeCount::default());
    let waker = Waker::from(count.clone());
    assert!(pl.poll_ready(&mut Context::from_waker(&waker)).is_pending());
    let checker = parked.borrow_mut().take().unwrap();
    // The check registers the checker waker as an observer of the same scope.
    for wakes in 1..=2 {
        assert!(pl.poll_ready(&mut Context::from_waker(&waker)).is_pending());
        expect_allocs("checker wake", 0, || checker.wake_by_ref());
        assert_eq!(count.0.load(Ordering::Relaxed), wakes);
    }
}

/// STATEMENT: Cached pipeline handles avoid readiness allocations and retain only call-dispatch allocation.
///
/// HOW:
/// - Warm readiness and measure direct, bound, cloned, owned, and trait-based entry points.
/// - Verify zero readiness allocations, one per call, notification rechecks, and shutdown behavior.
#[test]
fn cached_pipeline_handles_skip_readiness_allocations() {
    let (svc, gate) = probe();
    let callback = svc.1.clone();
    let pl = Pipeline::new(10, svc);
    let binding = pl.bind();
    let alias = binding.clone();
    pl.on_notify(&callback);
    binding.on_notify(&callback);
    alias.on_notify(&callback.clone());
    assert_eq!(complete(pl.ready()), Ok(()));

    for _ in 0..4 {
        assert_eq!(expect_allocs("ready", 0, || complete(pl.ready())), Ok(()));
        assert_eq!(
            expect_allocs("binding ready", 0, || complete(binding.ready())),
            Ok(())
        );
        assert_eq!(
            expect_allocs("alias ready", 0, || complete(alias.ready())),
            Ok(())
        );
        assert_eq!(
            expect_allocs("poll_ready", 0, || {
                pl.poll_ready(&mut Context::from_waker(Waker::noop()))
            }),
            Poll::Ready(Ok(()))
        );
        assert_eq!(expect_allocs("call", 1, || complete(pl.call(2))), Ok(12));
        assert_eq!(
            expect_allocs("binding call", 1, || complete(binding.call(2))),
            Ok(12)
        );
        assert_eq!(
            expect_allocs("call_static", 1, || complete(pl.call_static(2))),
            Ok(12)
        );
        assert_eq!(
            expect_allocs("binding call_static", 1, || complete(alias.call_static(2))),
            Ok(12)
        );
        assert_eq!(
            expect_allocs("ServiceCaller", 1, || complete(pl.call_service(2))),
            Ok(12)
        );
    }
    assert_eq!(gate.checks.get(), 1);
    assert_eq!(gate.notifications.get(), 0);

    let deferred = binding.ready();
    gate.changed.notify();
    assert_eq!(gate.notifications.get(), 1);
    assert_eq!(complete(deferred), Ok(()));
    assert_eq!(gate.checks.get(), 2);

    complete(pl.shutdown());
    gate.available.set(false);
    assert_eq!(
        expect_allocs("ready after shutdown", 0, || complete(pl.ready())),
        Ok(())
    );
    assert_eq!(gate.checks.get(), 2);
    assert_eq!(
        gate.notifications.get(),
        1,
        "shutdown is not a notification"
    );
}

/// STATEMENT: State-taking and fixed-state handles share allocation-free cached readiness.
///
/// HOW:
/// - Warm a `PipelineState` and measure readiness and calls across state values and handle types.
/// - Check dispatch-only call allocations, revalidation, and callback lifetime after handle drops.
#[test]
fn cached_state_handles_skip_readiness_allocations() {
    let (svc, gate) = probe();
    let callback = svc.1.clone();
    let pl = PipelineState::new(svc);
    let binding = pl.bind();
    let alias = binding.clone();
    let attached = pl.bind_state(20);
    let attached_alias = attached.clone();
    pl.on_notify(&callback);
    binding.on_notify(&callback);
    alias.on_notify(&callback.clone());
    attached.on_notify(&callback);
    attached_alias.on_notify(&callback);
    assert_eq!(complete(pl.ready(&10)), Ok(()));

    for st in [10, 20, 30, 40] {
        assert_eq!(
            expect_allocs("state ready", 0, || complete(pl.ready(&st))),
            Ok(())
        );
        assert_eq!(
            expect_allocs("attached ready", 0, || complete(attached.ready())),
            Ok(())
        );
        assert_eq!(
            expect_allocs("attached alias ready", 0, || complete(
                attached_alias.ready()
            )),
            Ok(())
        );
        assert_eq!(
            expect_allocs("state poll_ready", 0, || {
                pl.poll_ready(&mut Context::from_waker(Waker::noop()), &st)
            }),
            Poll::Ready(Ok(()))
        );
        assert_eq!(
            expect_allocs("state call", 1, || complete(pl.call(2, &st))),
            Ok(st + 2)
        );
        assert_eq!(
            expect_allocs("state binding call", 1, || complete(binding.call(2, &st))),
            Ok(st + 2)
        );
        assert_eq!(
            expect_allocs("state alias call", 1, || complete(alias.call(2, &st))),
            Ok(st + 2)
        );
        assert_eq!(
            expect_allocs("attached call", 1, || complete(attached.call(2))),
            Ok(22)
        );
        assert_eq!(
            expect_allocs("attached call_static", 1, || {
                complete(attached_alias.call_static(2))
            }),
            Ok(22)
        );
    }
    assert_eq!(gate.checks.get(), 1);
    assert_eq!(gate.notifications.get(), 0);

    let deferred = pl.ready(&50);
    gate.changed.notify();
    assert_eq!(gate.notifications.get(), 1);
    assert_eq!(complete(deferred), Ok(()));
    assert_eq!(gate.checks.get(), 2);
    assert_eq!(
        expect_allocs("attached ready after invalidation", 0, || {
            complete(attached.ready())
        }),
        Ok(())
    );
    drop((binding, alias, attached, attached_alias, callback));
    gate.changed.notify();
    assert_eq!(gate.notifications.get(), 2, "registrations outlive handles");
    complete(pl.shutdown(&50));
    assert_eq!(gate.notifications.get(), 2);
}

struct Forward<S>(S);

impl<S: Service<usize, usize>> Service<usize, usize> for Forward<S> {
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind(&self.0);
    }

    async fn ready(&self, ctx: Ctx<'_, Self, usize>) -> Result<(), S::Error> {
        ctx.ready(&self.0).await
    }

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<S::Res, S::Error> {
        ctx.ready(&self.0).await?;
        ctx.call(&self.0, req).await
    }
}

/// STATEMENT: Cached context forwarding adds no allocations beyond the pipeline and boxed call dispatches.
///
/// HOW:
/// - Warm nested readiness/state/mapping wrappers and an erased-service wrapper.
/// - Verify zero readiness allocations, expected call-box counts, and one underlying readiness check.
#[test]
fn cached_context_and_wrapper_forwarding_add_no_allocations() {
    let (svc, gate) = probe();
    let child = ready(map_state(7usize, ServiceBinding::new(svc))).map(|res| res + 1);
    let pl = Pipeline::new(10, Forward(child));
    assert_eq!(complete(pl.ready()), Ok(()));
    for _ in 0..4 {
        assert_eq!(
            expect_allocs("forwarded ready", 0, || complete(pl.ready())),
            Ok(())
        );
        assert_eq!(
            expect_allocs("forwarded Ctx::ready/call", 1, || complete(pl.call(2))),
            Ok(10)
        );
    }
    assert_eq!(gate.checks.get(), 1);

    let (svc, gate) = probe();
    let pl = Pipeline::new(10, Forward(ready(boxed::service(svc))));
    assert_eq!(complete(pl.ready()), Ok(()));
    assert_eq!(
        expect_allocs("ready around erased service", 0, || complete(pl.ready())),
        Ok(())
    );
    // Pipeline dispatch and BoxService dispatch each box their call future.
    assert_eq!(
        expect_allocs("erased Ctx::ready/call", 2, || complete(pl.call(2))),
        Ok(12)
    );
    assert_eq!(gate.checks.get(), 1);
}

/// STATEMENT: Invalidation and cancellation preserve one active checker with allocation-free repolls.
///
/// HOW:
/// - Measure cold checks, rechecks, pending owner/waiter polls, and owner cancellation.
/// - Verify checker handoff, retained-checker survival after waiter cancellation, and restored cache behavior.
#[test]
fn invalidation_and_cancellation_preserve_the_shared_checker() {
    let (svc, gate) = probe();
    let pl = Pipeline::new(10, svc);
    let binding = pl.bind();
    let (result, cold) = measure(|| complete(pl.ready()));
    assert_eq!(result, Ok(()));

    gate.changed.notify();
    let (result, recheck) = measure(|| complete(binding.ready()));
    assert_eq!(result, Ok(()));
    assert_eq!(recheck.allocs, 1);
    assert_eq!(gate.checks.get(), 2);
    assert_eq!(
        expect_allocs("rechecked cache", 0, || complete(pl.ready())),
        Ok(())
    );

    gate.available.set(false);
    gate.changed.notify();
    let mut cx = Context::from_waker(Waker::noop());
    let mut owner = pin!(Some(pl.ready()));
    let (result, pending) = measure(|| owner.as_mut().as_pin_mut().unwrap().poll(&mut cx));
    assert!(result.is_pending());
    assert_eq!(gate.active.get(), 1);
    for _ in 0..32 {
        assert!(
            expect_allocs("pending owner repoll", 0, || {
                owner.as_mut().as_pin_mut().unwrap().poll(&mut cx)
            })
            .is_pending()
        );
    }

    let mut waiter = pin!(binding.ready());
    let (result, waiting) = measure(|| waiter.as_mut().poll(&mut cx));
    assert!(result.is_pending());
    assert_eq!(gate.checks.get(), 3);
    assert!(
        expect_allocs("pending waiter repoll", 0, || waiter.as_mut().poll(&mut cx)).is_pending()
    );
    let (_, cancellation) = measure(|| owner.set(None));
    assert!(cancellation.deallocs > 0);
    assert_eq!(gate.active.get(), 0);
    assert_eq!(
        gate.notifications.get(),
        2,
        "cancellation is not a notification"
    );
    assert!(waiter.as_mut().poll(&mut cx).is_pending());
    assert_eq!(gate.active.get(), 1);
    assert_eq!(gate.checks.get(), 4);

    gate.available.set(true);
    gate.changed.notify();
    assert_eq!(waiter.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(gate.active.get(), 0);
    assert_eq!(
        expect_allocs("cache after handoff", 0, || complete(pl.ready())),
        Ok(())
    );
    assert_eq!(
        expect_allocs("call after handoff", 1, || complete(pl.call(2))),
        Ok(12)
    );

    gate.available.set(false);
    gate.changed.notify();
    let (result, retained) = measure(|| pl.poll_ready(&mut cx));
    assert!(result.is_pending());
    for _ in 0..32 {
        assert!(
            expect_allocs("retained checker repoll", 0, || pl.poll_ready(&mut cx)).is_pending()
        );
    }
    let checks = gate.checks.get();
    let mut waiter = pin!(Some(binding.ready()));
    assert!(
        waiter
            .as_mut()
            .as_pin_mut()
            .unwrap()
            .poll(&mut cx)
            .is_pending()
    );
    waiter.set(None);
    assert_eq!(
        gate.active.get(),
        1,
        "cancelling a waiter retains the checker"
    );
    assert_eq!(gate.checks.get(), checks);
    gate.available.set(true);
    gate.changed.notify();
    assert_eq!(pl.poll_ready(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(gate.active.get(), 0);
    assert_eq!(
        expect_allocs("retained checker cache", 0, || complete(pl.ready())),
        Ok(())
    );
    assert_eq!(gate.notifications.get(), 5);

    eprintln!(
        "cold={cold:?}; recheck={recheck:?}; pending={pending:?}; \
         waiter={waiting:?}; cancellation={cancellation:?}; retained={retained:?}"
    );
}

struct Linked(ntex_service::pipeline::PipelineBinding<usize, usize, ()>);

impl Service<usize, usize> for Linked {
    type Res = usize;
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe_pipeline(&self.0.readiness_link());
    }

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), ()> {
        self.0.ready().await
    }

    async fn call(&self, req: usize, _: Ctx<'_, Self, usize>) -> Result<usize, ()> {
        self.0.call(req).await
    }
}

/// STATEMENT: Parents sharing a child pipeline reuse its readiness cache without per-parent cache-hit allocation.
///
/// HOW:
/// - Link 16 parent pipelines to one state-taking child and measure repeated readiness and calls.
/// - Require zero readiness allocations and only the two pipeline dispatch boxes for calls.
/// - Invalidate the child without allocations, verify all parents become unready, and observe one child recheck.
#[test]
fn shared_child_cache_hits_do_not_allocate_per_parent() {
    let (svc, gate) = probe();
    let child = PipelineState::new(svc);
    let parents: Vec<_> = (0..16)
        .map(|st| Pipeline::new(st, Linked(child.bind_state(st))))
        .collect();
    for parent in &parents {
        complete(parent.ready()).unwrap();
    }
    assert_eq!(gate.checks.get(), 1);

    for _ in 0..4 {
        for (st, parent) in parents.iter().enumerate() {
            expect_allocs("shared child cached ready", 0, || {
                complete(parent.ready()).unwrap();
            });
            assert_eq!(
                expect_allocs("shared child cached call", 2, || complete(parent.call(2))),
                Ok(st + 2)
            );
        }
    }
    assert_eq!(gate.checks.get(), 1);

    expect_allocs("notify linked parents", 0, || gate.changed.notify());
    assert!(parents.iter().all(|parent| !parent.is_ready()));
    for parent in &parents {
        complete(parent.ready()).unwrap();
    }
    assert_eq!(gate.checks.get(), 2, "one shared child recheck");
    for parent in &parents {
        expect_allocs("shared child revalidated cache", 0, || {
            complete(parent.ready()).unwrap();
        });
    }
}

/// STATEMENT: Readiness timing comparisons retain cache-sharing and checker-handoff invariants.
///
/// HOW:
/// - Report seven warmed allocation/timing samples for cached, invalidated, shared, and pending operations.
/// - Assert readiness counts and cancellation handoff without imposing timing thresholds.
#[test]
#[ignore = "run the readiness microbenchmark explicitly in release mode"]
fn benchmark_readiness() {
    use std::{
        hint::black_box,
        time::{Duration, Instant},
    };

    fn bench<R>(name: &str, iterations: usize, mut operation: impl FnMut() -> R) -> usize {
        const SAMPLES: usize = 7;
        for _ in 0..iterations / 10 {
            black_box(operation());
        }
        let mut elapsed = [Duration::ZERO; SAMPLES];
        let mut counts = Counts::default();
        for sample in &mut elapsed {
            let (time, measured) = measure(|| {
                let start = Instant::now();
                for _ in 0..iterations {
                    black_box(operation());
                }
                start.elapsed()
            });
            *sample = time;
            counts.allocs += measured.allocs;
            counts.bytes += measured.bytes;
        }
        elapsed.sort_unstable();
        let ns = |time: Duration| time.as_secs_f64() * 1e9 / iterations as f64;
        let total = (iterations * SAMPLES) as f64;
        println!(
            "{name:32} {:9.2} ns/op [{:9.2}, {:9.2}] {:7.3} alloc/op {:9.2} B/op",
            ns(elapsed[SAMPLES / 2]),
            ns(elapsed[0]),
            ns(elapsed[SAMPLES - 1]),
            counts.allocs as f64 / total,
            counts.bytes as f64 / total,
        );
        iterations * SAMPLES + iterations / 10
    }

    println!(
        "median [min, max] of 7 samples; System allocator; 10% warmup; debug_assertions={}",
        cfg!(debug_assertions)
    );
    const ITERATIONS: usize = 1_000_000;
    let parked = Rc::new(RefCell::new(None));
    bench("pipeline/new+drop", ITERATIONS, || {
        black_box(Pipeline::new((), Parked(parked.clone())));
    });
    let (svc, gate) = probe();
    let pl = Pipeline::new(10, svc);
    let binding = pl.bind();
    let mut cx = Context::from_waker(Waker::noop());
    complete(pl.ready()).unwrap();
    bench("ready/cache-hit", ITERATIONS, || {
        complete(black_box(&pl).ready()).unwrap();
    });
    bench("poll-ready/cache-hit", ITERATIONS, || {
        assert_eq!(black_box(&pl).poll_ready(&mut cx), Poll::Ready(Ok(())));
    });
    bench("call/cache-hit", ITERATIONS, || {
        complete(black_box(&pl).call(black_box(2))).unwrap()
    });
    bench("binding-call/cache-hit", ITERATIONS, || {
        complete(black_box(&binding).call(black_box(2))).unwrap()
    });
    bench("owned-call/cache-hit", ITERATIONS, || {
        complete(black_box(&binding).call_static(black_box(2))).unwrap()
    });
    assert_eq!(gate.checks.get(), 1, "calls must not consume readiness");

    let operations = bench("notify+ready", ITERATIONS, || {
        black_box(&gate.changed).notify();
        complete(black_box(&pl).ready()).unwrap();
    });
    assert_eq!(gate.checks.get(), operations + 1);
    let checks = gate.checks.get();
    let operations = bench("notify+call", ITERATIONS, || {
        black_box(&gate.changed).notify();
        complete(black_box(&pl).call(black_box(2))).unwrap()
    });
    assert_eq!(gate.checks.get(), checks + operations);

    let (svc, gate) = probe();
    let state = PipelineState::new(svc);
    complete(state.ready(&10)).unwrap();
    bench("state-ready/cache-hit", ITERATIONS, || {
        complete(black_box(&state).ready(black_box(&20))).unwrap();
    });
    bench("state-call/cache-hit", ITERATIONS, || {
        complete(black_box(&state).call(black_box(2), black_box(&20))).unwrap()
    });
    assert_eq!(gate.checks.get(), 1, "state handles share readiness");

    let (svc, gate) = probe();
    let mapped = Pipeline::new(
        10,
        Forward(ready(map_state(7usize, ServiceBinding::new(svc))).map(|res| res + 1)),
    );
    complete(mapped.ready()).unwrap();
    bench("mapped-child/call/cache-hit", ITERATIONS, || {
        complete(black_box(&mapped).call(black_box(2))).unwrap()
    });
    assert_eq!(gate.checks.get(), 1);

    let (svc, gate) = probe();
    let erased = Pipeline::new(10, Forward(ready(boxed::service(svc))));
    complete(erased.ready()).unwrap();
    bench("erased-child/call/cache-hit", ITERATIONS, || {
        complete(black_box(&erased).call(black_box(2))).unwrap()
    });
    assert_eq!(gate.checks.get(), 1);

    for count in [1, 8, 64] {
        let (svc, gate) = probe();
        let child = PipelineState::new(svc);
        let parents: Vec<_> = (0..count)
            .map(|st| Pipeline::new(st, Linked(child.bind_state(st))))
            .collect();
        for parent in &parents {
            complete(parent.ready()).unwrap();
        }
        let operations = bench(&format!("notify+ready/{count}-parent-dag"), 10_000, || {
            black_box(&gate.changed).notify();
            for parent in black_box(&parents) {
                complete(parent.ready()).unwrap();
            }
        });
        assert_eq!(
            gate.checks.get(),
            operations + 1,
            "one child per generation"
        );
    }

    let (svc, gate) = probe();
    gate.available.set(false);
    let pl = Pipeline::new(10, svc);
    let bindings: Vec<_> = (0..8).map(|_| pl.bind()).collect();
    let mut owner = pin!(Some(pl.ready()));
    assert!(
        owner
            .as_mut()
            .as_pin_mut()
            .unwrap()
            .poll(&mut cx)
            .is_pending()
    );
    let mut waiters: Vec<_> = bindings.iter().map(|b| Box::pin(b.ready())).collect();
    for waiter in &mut waiters {
        assert!(waiter.as_mut().poll(&mut cx).is_pending());
    }
    bench("pending/owner-repoll", ITERATIONS, || {
        assert!(
            black_box(&mut owner)
                .as_mut()
                .as_pin_mut()
                .unwrap()
                .poll(&mut cx)
                .is_pending()
        );
    });
    bench("pending/8-waiter-repolls", ITERATIONS, || {
        for waiter in black_box(&mut waiters) {
            assert!(waiter.as_mut().poll(&mut cx).is_pending());
        }
    });
    assert_eq!(gate.checks.get(), 1);
    owner.set(None);
    assert!(waiters[0].as_mut().poll(&mut cx).is_pending());
    assert_eq!(
        gate.checks.get(),
        2,
        "cancelled owner hands off one checker"
    );
    gate.available.set(true);
    gate.changed.notify();
    for waiter in &mut waiters {
        assert_eq!(waiter.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    }
    assert_eq!(gate.active.get(), 0);

    let (svc, gate) = probe();
    gate.available.set(false);
    let pl = Pipeline::new(10, svc);
    assert!(pl.poll_ready(&mut cx).is_pending());
    bench("pending/retained-repoll", ITERATIONS, || {
        assert!(black_box(&pl).poll_ready(&mut cx).is_pending());
    });
    assert_eq!(gate.checks.get(), 1);
    gate.available.set(true);
    gate.changed.notify();
    assert_eq!(pl.poll_ready(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(gate.active.get(), 0);
}
