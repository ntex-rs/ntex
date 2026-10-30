use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::{cell::Cell, cell::RefCell, future::Future, future::poll_fn, pin::pin, rc::Rc};

use super::*;
use crate::{
    Pipeline, ReadyCallback, ReadyNotifier, ServiceCaller, ctx::WaitersRef,
    pipeline::PipelineState, ready,
};

thread_local! {
    static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
}

#[derive(Clone)]
struct State(usize);

#[derive(Debug, PartialEq, Eq)]
struct Error;

#[derive(Default)]
struct ProbeState {
    changed: ReadyNotifier,
    call_changed: ReadyNotifier,
    shutdown_changed: ReadyNotifier,
    on_notify: Option<ReadyCallback>,
    available: Cell<bool>,
    fail: Cell<bool>,
    call_fail: Cell<bool>,
    invalidate_call: Cell<bool>,
    invalidate_ready: Cell<bool>,
    invalidate_drop: Cell<bool>,
    binds: Cell<usize>,
    starts: Cell<usize>,
    active: Cell<usize>,
    max_active: Cell<usize>,
    drops: Cell<usize>,
    calls: Cell<usize>,
    states: RefCell<Vec<usize>>,
    waker: RefCell<Option<Waker>>,
    ready_hook: RefCell<Option<Box<dyn FnOnce()>>>,
    shutdown_state: Cell<usize>,
}

#[derive(Clone)]
struct Probe(Rc<ProbeState>);

impl Probe {
    fn new() -> Self {
        let state = ProbeState::default();
        state.available.set(true);
        Self(Rc::new(state))
    }
}

struct PendingCheck<'a>(&'a ProbeState);

impl Drop for PendingCheck<'_> {
    fn drop(&mut self) {
        self.0.active.set(self.0.active.get() - 1);
        self.0.drops.set(self.0.drops.get() + 1);
        if self.0.invalidate_drop.replace(false) {
            self.0.changed.notify();
        }
    }
}

impl Service<State, usize> for Probe {
    type Res = usize;
    type Error = Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        self.0.binds.set(self.0.binds.get() + 1);
        ctx.subscribe(&self.0.changed);
        ctx.subscribe(&self.0.call_changed);
        ctx.subscribe(&self.0.shutdown_changed);
        if let Some(callback) = &self.0.on_notify {
            ctx.on_notify(callback);
        }
    }

    fn ready(&self, ctx: Ctx<'_, Self, State>) -> impl Future<Output = Result<(), Error>> {
        self.0.starts.set(self.0.starts.get() + 1);
        self.0.active.set(self.0.active.get() + 1);
        self.0
            .max_active
            .set(self.0.max_active.get().max(self.0.active.get()));
        self.0.states.borrow_mut().push(ctx.st().0);
        let check = PendingCheck(&self.0);
        async move {
            let _check = check;
            poll_fn(|cx| {
                if !self.0.available.get() {
                    *self.0.waker.borrow_mut() = Some(cx.waker().clone());
                    Poll::Pending
                } else if self.0.fail.get() {
                    Poll::Ready(Err(Error))
                } else {
                    if self.0.invalidate_ready.replace(false) {
                        self.0.changed.notify();
                    }
                    let hook = self.0.ready_hook.borrow_mut().take();
                    if let Some(hook) = hook {
                        hook();
                    }
                    Poll::Ready(Ok(()))
                }
            })
            .await
        }
    }

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, State>) -> Result<usize, Error> {
        self.0.calls.set(self.0.calls.get() + 1);
        if self.0.invalidate_call.replace(false) {
            self.0.available.set(false);
            self.0.call_changed.notify();
        }
        if self.0.call_fail.get() {
            Err(Error)
        } else {
            Ok(req + ctx.st().0)
        }
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, State>) {
        self.0.shutdown_state.set(ctx.st().0);
    }
}

fn run<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("expected immediate completion"),
    }
}

#[derive(Default)]
struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
        let hook = ON_WAKE.with_borrow_mut(Option::take);
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[derive(Clone)]
struct Recover(Probe);

impl Service<State, Result<usize, Error>> for Recover {
    type Res = usize;
    type Error = Error;

    crate::forward_bind!(State, self.0);

    async fn ready(&self, ctx: Ctx<'_, Self, State>) -> Result<(), Error> {
        self.0.ready(ctx.cast()).await
    }

    async fn call(
        &self,
        req: Result<usize, Error>,
        ctx: Ctx<'_, Self, State>,
    ) -> Result<usize, Error> {
        self.0.call(req.unwrap_or(0), ctx.cast()).await
    }
}

fn compose_probes(
    a: Probe,
    b: Probe,
    use_then: bool,
) -> crate::boxed::BoxService<State, usize, usize, Error> {
    if use_then {
        crate::boxed::service(crate::then::Then::new(ready(a), ready(Recover(b))))
    } else {
        crate::boxed::service(crate::and_then::AndThen::new(ready(a), ready(b)))
    }
}

/// STATEMENT: Bound callbacks observe direct and propagated notifications, not calls or scheduling wakes.
///
/// HOW:
/// - Register one callback through services and pipeline aliases with and without a readiness wrapper.
/// - Check per-scope notification counts across pending checks, handle drops, and shutdown.
#[test]
fn bound_callbacks_observe_their_scope_and_propagated_notifications() {
    for wrapped in [false, true] {
        let calls = Rc::new(Cell::new(0));
        let captured = calls.clone();
        let callback = ReadyCallback::new(move || captured.set(captured.get() + 1));
        let probe = Probe(Rc::new(ProbeState {
            on_notify: Some(callback.clone()),
            available: Cell::new(true),
            ..ProbeState::default()
        }));
        let service = if wrapped {
            crate::boxed::service(ready(probe.clone()))
        } else {
            crate::boxed::service(probe.clone())
        };
        let pl = Pipeline::new(State(10), service);
        let binding = pl.bind();
        pl.on_notify(&callback);
        binding.on_notify(&callback.clone());
        let per_notify = if wrapped { 2 } else { 1 };

        assert_eq!(run(pl.ready()), Ok(()));
        assert_eq!(run(binding.call(1)), Ok(11));
        assert_eq!(calls.get(), 0);
        probe.0.changed.notify();
        assert_eq!(calls.get(), per_notify);
        assert_eq!(run(pl.ready()), Ok(()));

        probe.0.available.set(false);
        probe.0.changed.notify();
        {
            let mut pending = pin!(binding.ready());
            assert!(
                pending
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            let waker = probe.0.waker.borrow_mut().take().unwrap();
            waker.wake();
        }
        assert_eq!(calls.get(), 2 * per_notify);
        drop(binding);
        probe.0.changed.notify();
        assert_eq!(calls.get(), 3 * per_notify);
        probe.0.available.set(true);
        probe.0.changed.notify();
        assert_eq!(run(pl.ready()), Ok(()));
        run(pl.shutdown());
        assert_eq!(calls.get(), 4 * per_notify);
    }
}

/// STATEMENT: Pipeline construction binds services once before any state-dependent runtime operation.
///
/// HOW:
/// - Construct state-taking and fixed-state pipelines and check bind counts before readiness.
/// - Exercise readiness, calls, and shutdown through their handles without additional binds.
#[test]
fn pipeline_construction_binds_without_state_or_runtime_operations() {
    let probe = Probe::new();
    let pl = PipelineState::new(probe.clone());
    assert_eq!(probe.0.binds.get(), 1);
    assert_eq!(probe.0.starts.get(), 0);
    let handle = pl.bind();
    let owned = pl.bind_state(State(20));
    assert_eq!(run(pl.ready(&State(10))), Ok(()));
    assert_eq!(run(handle.call(1, &State(30))), Ok(31));
    assert_eq!(run(owned.call(2)), Ok(22));
    run(pl.shutdown(&State(40)));
    assert_eq!(probe.0.binds.get(), 1);

    let pl = Pipeline::new(State(50), probe.clone());
    assert_eq!(probe.0.binds.get(), 2);
    assert_eq!(run(pl.bind().call(3)), Ok(53));
    run(pl.shutdown());
    assert_eq!(probe.0.binds.get(), 2);
}

/// STATEMENT: Lazy creation binds the nearest scope and invalidates ancestors without disturbing siblings.
///
/// HOW:
/// - Create raw and readiness-wrapped children through a mapped-state context under a ready parent.
/// - Verify binding precedes use, creation resets do not notify, and child notifications reach ancestors.
#[test]
fn lazy_creation_binds_the_nearest_scope_before_first_use() {
    for wrapped in [false, true] {
        let waiters = WaitersRef::new();
        let st = State(10);
        let mapped = State(20);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let parent = ServiceBinding::new(Probe::new());
        let sibling = ServiceBinding::new(Probe::new());
        parent.bind(BindCtx::new(waiters.scope()));
        sibling.bind(BindCtx::new(waiters.scope()));
        let notifications = Rc::new(Cell::new(0));
        let captured = notifications.clone();
        BindCtx::new(waiters.scope()).on_notify(&ReadyCallback::new(move || {
            captured.set(captured.get() + 1);
        }));
        assert_eq!(run(ctx.ready(&parent)), Ok(()));
        assert_eq!(run(ctx.ready(&sibling)), Ok(()));
        assert!(waiters.scope().set_ready(waiters.scope().generation()));

        let child = Probe::new();
        let probe = child.clone();
        let factory = crate::fn_factory(move |st: &State| {
            assert_eq!(st.0, 20);
            let probe = probe.clone();
            async move {
                Ok::<_, Error>(if wrapped {
                    crate::boxed::service(ready(probe))
                } else {
                    crate::boxed::service(probe)
                })
            }
        });
        let ctx = ctx.with_scope::<()>(&parent.scope);
        let ctx = ctx.map_state(&mapped);
        let service = run(ctx.create(&factory)).unwrap();
        assert_eq!(child.0.binds.get(), 1);
        assert_eq!(child.0.starts.get(), 0);
        assert!(!parent.is_ready());
        assert!(!waiters.scope().is_ready());
        assert!(sibling.is_ready());
        assert_eq!(
            notifications.get(),
            0,
            "creation resets are not notifications"
        );

        assert!(parent.scope.set_ready(parent.scope.generation()));
        assert!(waiters.scope().set_ready(waiters.scope().generation()));
        child.0.changed.notify();
        assert_eq!(notifications.get(), 1);
        assert!(!parent.is_ready());
        assert!(!waiters.scope().is_ready());
        assert!(sibling.is_ready());
        assert_eq!(run(ctx.call(&service, 1)), Ok(21));
        assert_eq!(child.0.starts.get(), 1);
        assert_eq!(child.0.binds.get(), 1);
        run(ctx.shutdown(&service));
        assert_eq!(child.0.shutdown_state.get(), 20);
    }
}

/// STATEMENT: Failed or cancelled lazy creation neither binds a child nor invalidates its parent.
///
/// HOW:
/// - Run a failing factory and drop a pending factory future under a ready scope.
/// - Notify the unused child and verify unchanged parent generation and zero bind/readiness counts.
#[test]
fn failed_or_cancelled_lazy_creation_does_not_bind_or_invalidate() {
    let waiters = WaitersRef::new();
    let st = State(10);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let child = Probe::new();
    let generation = waiters.scope().generation();
    assert!(waiters.scope().set_ready(generation));

    let failed = crate::fn_factory(async |_: &State| Err::<Probe, _>("init error"));
    assert!(matches!(run(ctx.create(&failed)), Err("init error")));
    assert!(waiters.scope().is_current(generation));
    assert!(waiters.scope().is_ready());

    let probe = child.clone();
    let pending = crate::fn_factory(move |_: &State| {
        let probe = probe.clone();
        async move {
            std::future::pending::<()>().await;
            Ok::<_, Error>(probe)
        }
    });
    let mut create = Box::pin(ctx.create(&pending));
    assert!(
        create
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(create);
    child.0.changed.notify();
    assert!(waiters.scope().is_current(generation));
    assert!(waiters.scope().is_ready());
    assert_eq!(child.0.binds.get(), 0);
    assert_eq!(child.0.starts.get(), 0);
}

/// STATEMENT: Binding establishes all notifier subscriptions and parent edges before readiness starts.
///
/// HOW:
/// - Bind two parents sharing one child into a root scope without polling readiness.
/// - Notify each child lifecycle source and verify every dependent generation changes.
#[test]
fn all_sources_and_parent_edges_are_live_before_first_readiness() {
    let waiters = WaitersRef::new();
    let probe = Probe::new();
    let child = Rc::new(ServiceBinding::new(probe.clone()));
    let left = ServiceBinding::new(Forward(child.clone()));
    let right = ServiceBinding::new(Forward(child.clone()));
    left.bind(BindCtx::new(waiters.scope()));
    right.bind(BindCtx::new(waiters.scope()));
    assert_eq!(probe.0.starts.get(), 0);
    assert_eq!(probe.0.binds.get(), 2);
    for source in [
        &probe.0.changed,
        &probe.0.call_changed,
        &probe.0.shutdown_changed,
    ] {
        let scopes = [&child.scope, &left.scope, &right.scope, waiters.scope()];
        let generations = scopes.map(ReadyScope::generation);
        source.notify();
        for (scope, generation) in scopes.into_iter().zip(generations) {
            assert!(!scope.is_current(generation));
        }
    }
    assert_eq!(probe.0.starts.get(), 0);
}

/// STATEMENT: Runtime operations reject an unbound parent rather than silently repairing its dependency.
///
/// HOW:
/// - Invoke ready, call, legacy nowait, and shutdown through fresh unregistered contexts.
/// - Catch the binding panic and verify no extra binds, calls, or parent subscriptions are introduced.
#[test]
#[allow(
    deprecated,
    reason = "verify the legacy bypass still rejects unbound scopes"
)]
fn runtime_operations_reject_missing_parent_binding_without_repair() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let probe = Probe::new();
    let child = Rc::new(ServiceBinding::new(probe.clone()));
    let pl = Pipeline::new(State(0), child.clone());
    assert_eq!(run(pl.ready()), Ok(()));
    for path in 0..4 {
        let waiters = WaitersRef::new();
        let st = State(0);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let result = catch_unwind(AssertUnwindSafe(|| match path {
            0 => run(ctx.ready(&child)).unwrap(),
            1 => {
                run(ctx.call(&child, 1)).unwrap();
            }
            2 => {
                run(ctx.call_nowait(&child, 1)).unwrap();
            }
            _ => run(ctx.shutdown(&child)),
        }));
        let panic = result.expect_err("unbound parent must be rejected");
        assert!(
            panic.downcast_ref::<String>().is_some_and(|message| {
                message.contains("ServiceBinding must be registered by Service::bind")
            }) || panic.downcast_ref::<&str>().is_some_and(|message| {
                message.contains("ServiceBinding must be registered by Service::bind")
            })
        );
        assert_eq!(probe.0.binds.get(), 1);
        assert_eq!(probe.0.calls.get(), 0);
        let generation = waiters.scope().generation();
        probe.0.changed.notify();
        assert!(waiters.scope().is_current(generation));
    }
}

/// STATEMENT: `Then` and `AndThen` cache wrapped children independently without consuming successful readiness.
///
/// HOW:
/// - Call each composition repeatedly and invalidate its children separately.
/// - Check independent readiness counts and the distinct recovery/short-circuit behavior for call errors.
#[test]
fn combinators_cache_children_independently_without_consuming_readiness() {
    for use_then in [false, true] {
        let waiters = WaitersRef::new();
        let st = State(10);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let a = Probe::new();
        let b = Probe::new();
        let parent = ServiceBinding::new(compose_probes(a.clone(), b.clone(), use_then));
        parent.bind(BindCtx::new(waiters.scope()));
        assert_eq!(run(ctx.call(&parent, 1)), Ok(21));
        assert_eq!(run(ctx.call(&parent, 2)), Ok(22));
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (1, 1));
        a.0.changed.notify();
        assert!(!parent.scope.is_ready());
        assert_eq!(run(ctx.call(&parent, 3)), Ok(23));
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (2, 1));
        b.0.changed.notify();
        assert!(!parent.scope.is_ready());
        assert_eq!(run(ctx.call(&parent, 4)), Ok(24));
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (2, 2));

        a.0.call_fail.set(true);
        let expected = if use_then { Ok(10) } else { Err(Error) };
        assert_eq!(run(ctx.call(&parent, 5)), expected);
        assert_eq!(b.0.calls.get(), if use_then { 5 } else { 4 });
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (2, 2));
    }
}

/// STATEMENT: Raw combinators cache readiness only when an enclosing pipeline provides a cache.
///
/// HOW:
/// - Repeatedly check raw `Then`, `AndThen`, and apply services directly through a context.
/// - Wrap them in a pipeline and verify cache reuse and notifier-triggered rechecks.
#[test]
fn raw_combinators_only_cache_readiness_in_the_enclosing_scope() {
    for path in 0..3 {
        let waiters = WaitersRef::new();
        let st = State(10);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let a = Probe::new();
        let b = Probe::new();
        let service = match path {
            0 => crate::boxed::service(crate::and_then::AndThen::new(a.clone(), b.clone())),
            1 => crate::boxed::service(crate::then::Then::new(a.clone(), Recover(b.clone()))),
            _ => crate::boxed::service(crate::apply_fn(b.clone(), async |req: usize, ctx| {
                ctx.call(req).await
            })),
        };
        service.bind(BindCtx::new(waiters.scope()));
        assert_eq!(run(ctx.ready(&service)), Ok(()));
        assert_eq!(run(ctx.ready(&service)), Ok(()));
        assert_eq!(b.0.starts.get(), 2);
        assert_eq!(a.0.starts.get(), if path == 2 { 0 } else { 2 });

        let pl = Pipeline::new(State(10), service);
        assert_eq!(run(pl.ready()), Ok(()));
        assert_eq!(run(pl.ready()), Ok(()));
        assert_eq!(b.0.starts.get(), 3);
        b.0.changed.notify();
        assert_eq!(run(pl.ready()), Ok(()));
        assert_eq!(b.0.starts.get(), 4);
        assert_eq!(a.0.starts.get(), if path == 2 { 0 } else { 4 });
        assert_eq!(run(pl.call(1)), Ok(if path == 2 { 11 } else { 21 }));
        assert_eq!(b.0.starts.get(), 4);
    }
}

/// STATEMENT: Raw and mixed aggregates reject a child's success invalidated by a later sibling poll.
///
/// HOW:
/// - Make the second child's readiness hook close and notify the first child.
/// - Verify pending readiness and a wake, then reopen the first child and check completion without overlap.
#[test]
fn raw_and_mixed_aggregates_reject_invalidated_sibling_success() {
    for wrapped_second in [false, true] {
        let waiters = WaitersRef::new();
        let st = State(0);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let a = Probe::new();
        let b = Probe::new();
        let changed = a.clone();
        *b.0.ready_hook.borrow_mut() = Some(Box::new(move || {
            changed.0.available.set(false);
            changed.0.changed.notify();
        }));
        let second = if wrapped_second {
            crate::boxed::service(ready(b.clone()))
        } else {
            crate::boxed::service(b.clone())
        };
        let service = crate::and_then::AndThen::new(a.clone(), second);
        service.bind(BindCtx::new(waiters.scope()));
        let mut ready = Box::pin(ctx.ready(&service));
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        assert!(counter.0.load(Ordering::Relaxed) > 0);
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        assert_eq!(a.0.calls.get(), 0);
        a.0.available.set(true);
        a.0.changed.notify();
        assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        assert_eq!((a.0.active.get(), b.0.active.get()), (0, 0));
        assert_eq!((a.0.max_active.get(), b.0.max_active.get()), (1, 1));
        assert_eq!(b.0.starts.get() == 1, wrapped_second);
    }
}

/// STATEMENT: Readiness factories create independent gates while aliases share state-aware readiness.
///
/// HOW:
/// - Check initialization failure, create two services, and compare checks through aliases and mapped state.
/// - Invalidate into readiness errors, recover, and verify call and shutdown state propagation.
#[test]
fn ready_factory_creates_fresh_gates_and_preserves_state_and_errors() {
    use crate::{ServiceFactory, fn_factory, ready_factory};

    let waiters = WaitersRef::new();
    let st = State(10);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let created = Rc::new(probe.clone());
    let factory = ready_factory(fn_factory(move |st: &State| {
        let created = created.clone();
        let fail = st.0 == 0;
        async move {
            if fail {
                Err("initialization failed")
            } else {
                Ok(created)
            }
        }
    }));
    assert!(matches!(
        run(factory.create(&State(0))),
        Err("initialization failed")
    ));
    let first = Rc::new(run(factory.create(&st)).unwrap());
    let alias = first.clone();
    let second = run(factory.clone().create(&st)).unwrap();
    first.bind(BindCtx::new(waiters.scope()));
    second.bind(BindCtx::new(waiters.scope()));
    assert_eq!(probe.0.starts.get(), 0);
    assert_eq!(run(ctx.ready(&first)), Ok(()));
    assert_eq!(run(ctx.ready(&alias)), Ok(()));
    assert_eq!(probe.0.starts.get(), 1);
    assert_eq!(run(ctx.ready(&second)), Ok(()));
    assert_eq!(probe.0.starts.get(), 2);
    assert_eq!(run(first.call(1, ctx.cast())), Ok(11));
    let mapped = crate::map_state(State(20), alias);
    BindCtx::new(waiters.scope()).bind::<_, State, usize>(&mapped);
    assert_eq!(run(mapped.call(2, ctx.cast())), Ok(22));
    assert_eq!(probe.0.starts.get(), 2);

    probe.0.fail.set(true);
    probe.0.changed.notify();
    assert_eq!(run(ctx.ready(&first)), Err(Error));
    assert_eq!(run(ctx.ready(&second)), Err(Error));
    assert_eq!(probe.0.starts.get(), 4);
    probe.0.fail.set(false);
    assert_eq!(run(first.call(3, ctx.cast())), Ok(13));
    assert_eq!(run(second.call(4, ctx.cast())), Ok(14));
    assert_eq!(probe.0.starts.get(), 6);
    assert!(probe.0.states.borrow().iter().all(|st| *st == 10));
    run(ctx.shutdown(&mapped));
    assert_eq!(probe.0.shutdown_state.get(), 20);
}

/// STATEMENT: Direct child calls can use completed child readiness while context calls still wait for the owner.
///
/// HOW:
/// - Keep an aggregate owner pending on one child while revalidating another child.
/// - Compare context, direct, erased/mapped, and child-scope call paths and verify their completion timing.
#[test]
fn waiting_child_calls_observe_cache_success_before_an_unrelated_check_finishes() {
    struct Both {
        a: ServiceBinding<Probe>,
        b: Rc<ServiceBinding<Probe>>,
    }

    impl Service<State, usize> for Both {
        type Res = usize;
        type Error = Error;

        crate::forward_bind!(State, a, b);

        async fn ready(&self, ctx: Ctx<'_, Self, State>) -> Result<(), Error> {
            crate::util::ready(&self.a, &self.b, ctx).await
        }

        async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Error> {
            Ok(req)
        }
    }

    for path in 0..4 {
        let waiters = WaitersRef::new();
        let st = State(0);
        let owner_ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let registration = waiters.register();
        let caller_ctx = Ctx::<(), State>::new(registration.idx, &waiters, &st);
        let a = Probe::new();
        let b = Probe::new();
        a.0.available.set(false);
        let pair = Both {
            a: ServiceBinding::new(a.clone()),
            b: Rc::new(ServiceBinding::new(b.clone())),
        };
        let erased = crate::boxed::service(crate::map_state(
            State(10),
            Box::new(pair.b.clone())
                .map(|res| res + 100)
                .map_err(|err| err),
        ));
        pair.bind(BindCtx::new(waiters.scope()));
        erased.bind(BindCtx::new(waiters.scope()));
        let mut owner = Box::pin(owner_ctx.ready(&pair));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(owner.as_mut().poll(&mut cx).is_pending());
        b.0.changed.notify();
        let mut call = Box::pin(async {
            match path {
                0 => caller_ctx.call(&pair.b, 1).await,
                1 => pair.b.call(1, caller_ctx.cast()).await,
                2 => erased.call(1, caller_ctx.cast()).await,
                _ => caller_ctx.with_scope::<()>(&pair.b.scope).call(&b, 1).await,
            }
        });
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut task = Context::from_waker(&waker);
        assert!(call.as_mut().poll(&mut task).is_pending());
        assert!(owner.as_mut().poll(&mut cx).is_pending());
        assert!(pair.b.is_ready());
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (1, 2));
        assert_eq!(a.0.drops.get(), 0);
        if path == 0 {
            assert!(call.as_mut().poll(&mut task).is_pending());
            a.0.available.set(true);
            a.0.changed.notify();
            assert_eq!(owner.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        } else {
            assert!(counter.0.load(Ordering::Relaxed) > 0);
        }
        let expected = if path == 2 { 111 } else { 1 };
        assert_eq!(call.as_mut().poll(&mut task), Poll::Ready(Ok(expected)));
    }
}

/// STATEMENT: Built-in calls use child caches independently of unrelated checks and clear nested legacy bypasses.
///
/// HOW:
/// - Call `Then`, `AndThen`, and apply services while another readiness owner remains pending.
/// - Invalidate child caches into errors and verify a legacy outer bypass still checks nested calls.
#[test]
#[allow(deprecated, reason = "verify nested calls clear the legacy bypass")]
fn built_in_calls_use_child_caches_while_an_unrelated_check_is_pending() {
    for path in 0..3 {
        let waiters = WaitersRef::new();
        let st = State(10);
        let owner_ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let registration = waiters.register();
        let caller_ctx = Ctx::<(), State>::new(registration.idx, &waiters, &st);
        let a = Probe::new();
        let b = Probe::new();
        let service = match path {
            0 => compose_probes(a.clone(), b.clone(), false),
            1 => compose_probes(a.clone(), b.clone(), true),
            _ => crate::boxed::service(crate::apply_fn(
                ready(b.clone()),
                async |req: usize, ctx| ctx.call_service(req).await,
            )),
        };
        service.bind(BindCtx::new(waiters.scope()));
        assert_eq!(run(owner_ctx.ready(&service)), Ok(()));
        assert!(!owner_ctx.scope().is_ready());

        let unrelated = Probe::new();
        unrelated.bind(BindCtx::new(waiters.scope()));
        unrelated.0.available.set(false);
        let mut owner = Box::pin(owner_ctx.ready(&unrelated));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(owner.as_mut().poll(&mut cx).is_pending());
        let expected = if path == 2 { 11 } else { 21 };
        assert_eq!(run(service.call(1, caller_ctx.cast())), Ok(expected));
        assert_eq!(b.0.starts.get(), 1);
        assert_eq!(b.0.calls.get(), 1);
        assert_eq!(unrelated.0.drops.get(), 0);
        drop(owner);

        a.0.fail.set(true);
        a.0.changed.notify();
        b.0.fail.set(true);
        b.0.changed.notify();
        assert_eq!(run(owner_ctx.call_nowait(&service, 2)), Err(Error));
        assert_eq!(a.0.starts.get(), usize::from(path != 2));
        assert_eq!(b.0.starts.get(), 2);
        assert_eq!(b.0.calls.get(), 1);
    }
}

/// STATEMENT: Aggregate readiness rechecks invalidated completed children without restarting pending siblings.
///
/// HOW:
/// - Exercise both combinators and both child orders with one completed and one pending check.
/// - Repeatedly invalidate the completed child, then release both and verify checker counts and exclusivity.
#[test]
fn aggregate_rechecks_completed_children_without_restarting_pending_siblings() {
    for use_then in [false, true] {
        for completed_first in [false, true] {
            let waiters = WaitersRef::new();
            let st = State(0);
            let ctx = Ctx::<(), State>::new(0, &waiters, &st);
            let a = Probe::new();
            let b = Probe::new();
            let (completed, pending) = if completed_first { (&a, &b) } else { (&b, &a) };
            pending.0.available.set(false);
            let parent = ServiceBinding::new(compose_probes(a.clone(), b.clone(), use_then));
            parent.bind(BindCtx::new(waiters.scope()));
            let mut ready = Box::pin(ctx.ready(&parent));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(ready.as_mut().poll(&mut cx).is_pending());
            assert_eq!((completed.0.starts.get(), pending.0.starts.get()), (1, 1));
            completed.0.available.set(false);
            completed.0.changed.notify();
            assert_eq!(pending.0.drops.get(), 0);
            assert!(ready.as_mut().poll(&mut cx).is_pending());
            assert_eq!((completed.0.starts.get(), pending.0.starts.get()), (2, 1));
            assert_eq!(pending.0.drops.get(), 0);
            assert!(!parent.scope.is_ready());

            completed.0.available.set(true);
            completed.0.changed.notify();
            assert!(ready.as_mut().poll(&mut cx).is_pending());
            assert_eq!((completed.0.starts.get(), pending.0.starts.get()), (3, 1));
            assert_eq!(pending.0.drops.get(), 0);
            pending.0.available.set(true);
            pending.0.changed.notify();
            assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
            assert_eq!(run(ctx.call(&parent, 1)), Ok(1));
            assert_eq!((completed.0.starts.get(), pending.0.starts.get()), (3, 2));
            assert_eq!((a.0.max_active.get(), b.0.max_active.get()), (1, 1));
        }
    }
}

/// STATEMENT: Aggregate success revalidates a child invalidated by a later child's readiness poll.
///
/// HOW:
/// - Make the second child close and notify the first during `Then` and `AndThen` readiness.
/// - Verify the parent stays unready and only the invalidated child is rechecked before completion.
#[test]
fn aggregate_success_revalidates_a_child_changed_by_a_later_poll() {
    for use_then in [false, true] {
        let waiters = WaitersRef::new();
        let st = State(0);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let a = Probe::new();
        let b = Probe::new();
        let changed = a.clone();
        *b.0.ready_hook.borrow_mut() = Some(Box::new(move || {
            changed.0.available.set(false);
            changed.0.changed.notify();
        }));
        let parent = ServiceBinding::new(compose_probes(a.clone(), b.clone(), use_then));
        parent.bind(BindCtx::new(waiters.scope()));
        let mut ready = Box::pin(ctx.ready(&parent));
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        assert!(!parent.scope.is_ready());
        assert!(counter.0.load(Ordering::Relaxed) > 0);
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (2, 1));
        a.0.available.set(true);
        a.0.changed.notify();
        assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (3, 1));
    }
}

/// STATEMENT: Aggregate cancellation and readiness errors release child ownership and preserve error precedence.
///
/// HOW:
/// - Join two aggregate checks, cancel their owner, and verify a woken waiter takes over both children.
/// - Fail the second child, recover, then fail the first and verify cleanup and short-circuiting.
#[test]
fn aggregate_cancellation_and_errors_release_each_child_checker() {
    for use_then in [false, true] {
        let waiters = WaitersRef::new();
        let other_waiters = WaitersRef::new();
        let st = State(0);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let other_ctx = Ctx::<(), State>::new(0, &other_waiters, &st);
        let a = Probe::new();
        let b = Probe::new();
        a.0.available.set(false);
        b.0.available.set(false);
        let service = compose_probes(a.clone(), b.clone(), use_then);
        service.bind(BindCtx::new(waiters.scope()));
        service.bind(BindCtx::new(other_waiters.scope()));
        let mut first = Box::pin(ctx.ready(&service));
        let mut second = Box::pin(other_ctx.ready(&service));
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (1, 1));
        let before = counter.0.load(Ordering::Relaxed);
        drop(first);
        assert!(counter.0.load(Ordering::Relaxed) > before);
        assert_eq!((a.0.active.get(), b.0.active.get()), (0, 0));
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (2, 2));

        b.0.available.set(true);
        b.0.fail.set(true);
        b.0.changed.notify();
        assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(Err(Error)));
        assert_eq!((a.0.active.get(), b.0.active.get()), (0, 0));
        assert_eq!((a.0.calls.get(), b.0.calls.get()), (0, 0));
        a.0.available.set(true);
        b.0.fail.set(false);
        a.0.changed.notify();
        b.0.changed.notify();
        assert_eq!(run(ctx.call(&service, 1)), Ok(1));
        assert_eq!((a.0.max_active.get(), b.0.max_active.get()), (1, 1));

        a.0.fail.set(true);
        a.0.changed.notify();
        b.0.changed.notify();
        let before = b.0.starts.get();
        assert_eq!(run(ctx.ready(&service)), Err(Error));
        assert_eq!(b.0.starts.get(), before, "the first readiness error wins");
    }
}

/// STATEMENT: Combinators recheck the second child's readiness after an awaited first call.
///
/// HOW:
/// - Pause the first call on a channel and invalidate the second child into an error.
/// - Resume and verify no second-child call occurs, then recover and check the next request succeeds.
#[test]
fn combinators_recheck_second_child_errors_after_awaiting_the_first_call() {
    use ntex::channel::oneshot::{self, Receiver};

    for use_then in [false, true] {
        let b = Probe::new();
        let first = crate::fn_service(async |(req, rx): (usize, Receiver<()>)| {
            let _ = rx.await;
            Ok::<_, Error>(req)
        });
        let service = if use_then {
            crate::boxed::service(crate::then::Then::new(first, ready(Recover(b.clone()))))
        } else {
            crate::boxed::service(crate::and_then::AndThen::new(first, ready(b.clone())))
        };
        let pl = Pipeline::new(State(10), service);
        let (tx, rx) = oneshot::channel();
        let mut call = Box::pin(pl.call((1, rx)));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut cx).is_pending());
        assert_eq!(b.0.starts.get(), 1);
        b.0.fail.set(true);
        b.0.changed.notify();
        tx.send(()).unwrap();
        assert_eq!(call.as_mut().poll(&mut cx), Poll::Ready(Err(Error)));
        assert_eq!(b.0.calls.get(), 0);
        assert_eq!(b.0.starts.get(), 2);

        b.0.fail.set(false);
        b.0.changed.notify();
        let (tx, rx) = oneshot::channel();
        tx.send(()).unwrap();
        assert_eq!(run(pl.call((2, rx))), Ok(12));
        assert_eq!(b.0.starts.get(), 3);
    }
}

/// STATEMENT: Cloned `Then`, `AndThen`, and apply services start with fresh child readiness caches.
///
/// HOW:
/// - Bind an original and clone of each composition and check readiness on both.
/// - Recheck the original and verify exactly two child checks rather than shared or discarded caches.
#[test]
fn combinator_clones_start_with_fresh_child_readiness() {
    let waiters = WaitersRef::new();
    let st = State(0);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    for use_then in [false, true] {
        let a = Probe::new();
        let b = Probe::new();
        if use_then {
            let service = crate::then::Then::new(ready(a.clone()), ready(Recover(b.clone())));
            service.bind(BindCtx::new(waiters.scope()));
            let fresh = service.clone();
            fresh.bind(BindCtx::new(waiters.scope()));
            assert_eq!(run(ctx.ready(&service)), Ok(()));
            assert_eq!(run(ctx.ready(&fresh)), Ok(()));
            assert_eq!(run(ctx.ready(&service)), Ok(()));
        } else {
            let service = crate::and_then::AndThen::new(ready(a.clone()), ready(b.clone()));
            service.bind(BindCtx::new(waiters.scope()));
            let fresh = service.clone();
            fresh.bind(BindCtx::new(waiters.scope()));
            assert_eq!(run(ctx.ready(&service)), Ok(()));
            assert_eq!(run(ctx.ready(&fresh)), Ok(()));
            assert_eq!(run(ctx.ready(&service)), Ok(()));
        }
        assert_eq!((a.0.starts.get(), b.0.starts.get()), (2, 2));
    }

    let probe = Probe::new();
    let service = crate::apply_fn(ready(probe.clone()), async |req: usize, ctx| {
        ctx.call(req).await
    });
    service.bind(BindCtx::new(waiters.scope()));
    let fresh = service.clone();
    fresh.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&service)), Ok(()));
    assert_eq!(run(ctx.ready(&fresh)), Ok(()));
    assert_eq!(run(ctx.ready(&service)), Ok(()));
    assert_eq!(probe.0.starts.get(), 2);
}

/// STATEMENT: Repeated apply-context calls reuse child readiness until an inner call invalidates it.
///
/// HOW:
/// - Invoke the inner service three times per request through both apply call interfaces.
/// - Invalidate during the next request and verify it pauses, rechecks, and resumes with the same state.
#[test]
fn apply_calls_reuse_readiness_until_the_inner_service_notifies() {
    let probe = Probe::new();
    let applied = crate::apply_fn(ready(probe.clone()), async |req: usize, ctx| {
        assert_eq!(ctx.st().0, 10);
        let first = ctx.call(req).await?;
        let second = ctx.call_service(first).await?;
        ctx.call(second).await
    });
    let pl = Pipeline::new(State(10), applied);
    assert_eq!(run(pl.call(1)), Ok(31));
    assert_eq!(probe.0.starts.get(), 1);
    assert_eq!(probe.0.calls.get(), 3);
    probe.0.invalidate_call.set(true);
    let mut next = Box::pin(pl.call(2));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(next.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.calls.get(), 4);
    assert_eq!(probe.0.starts.get(), 2);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(next.as_mut().poll(&mut cx), Poll::Ready(Ok(32)));
    assert_eq!(probe.0.calls.get(), 6);
    assert_eq!(probe.0.starts.get(), 3);
    assert!(probe.0.states.borrow().iter().all(|st| *st == 10));
}

/// STATEMENT: All raw-root pipeline call entry points share readiness until notification.
///
/// HOW:
/// - Mix direct, bound, trait-based, and owned calls and verify one initial check.
/// - Invalidate during a call and verify deferred owned calls share revalidation before dispatch.
#[test]
fn pipeline_raw_roots_cache_all_call_entry_points_until_notification() {
    let probe = Probe::new();
    let pl = Pipeline::new(State(10), probe.clone());
    let binding = pl.bind();
    assert_eq!(run(pl.call(1)), Ok(11));
    assert_eq!(run(pl.ready()), Ok(()));
    assert_eq!(run(binding.ready()), Ok(()));
    assert_eq!(run(binding.call(2)), Ok(12));
    assert_eq!(run(ServiceCaller::call_service(&pl, 3)), Ok(13));
    assert_eq!(run(pl.call_static(4)), Ok(14));
    assert_eq!(run(binding.call_static(5)), Ok(15));
    assert_eq!(
        probe.0.starts.get(),
        1,
        "root dispatch must not raw-check again"
    );

    let mut owned = Box::pin(pl.call_static(6));
    let mut bound_owned = Box::pin(binding.call_static(7));
    probe.0.invalidate_call.set(true);
    assert_eq!(run(pl.call(8)), Ok(18));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(owned.as_mut().poll(&mut cx).is_pending());
    assert!(bound_owned.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.calls.get(), 6);
    assert_eq!(probe.0.starts.get(), 2);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(owned.as_mut().poll(&mut cx), Poll::Ready(Ok(16)));
    assert_eq!(bound_owned.as_mut().poll(&mut cx), Poll::Ready(Ok(17)));
    assert_eq!(probe.0.starts.get(), 3);
    assert_eq!(probe.0.max_active.get(), 1);
    assert!(probe.0.states.borrow().iter().all(|st| *st == 10));
}

/// STATEMENT: Pipeline-owned readiness checkers reject remote wakes but remain usable locally.
///
/// HOW:
/// - Park fixed-state and state-taking pipeline readiness and wake each checker from another thread.
/// - Verify the remote panic and no observer wake, then complete with a local wake and one service check.
#[test]
fn pipeline_owned_readiness_rejects_remote_wakes() {
    for stateful in [false, true] {
        let probe = Probe::new();
        probe.0.available.set(false);
        let pl = Pipeline::new(State(10), probe.clone());
        let state_pl = PipelineState::new(probe.clone());
        let poll = |cx: &mut Context<'_>| {
            if stateful {
                state_pl.poll_ready(cx, &State(10))
            } else {
                pl.poll_ready(cx)
            }
        };
        let counter = Arc::new(Counter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(poll(&mut cx).is_pending());
        let checker = probe.0.waker.borrow_mut().take().unwrap();
        let remote = checker.clone();
        assert!(std::thread::spawn(move || remote.wake()).join().is_err());
        assert_eq!(counter.0.load(Ordering::Relaxed), 0);

        probe.0.available.set(true);
        checker.wake();
        assert_eq!(counter.0.load(Ordering::Relaxed), 1);
        assert_eq!(poll(&mut cx), Poll::Ready(Ok(())));
        assert_eq!(probe.0.starts.get(), 1);
        assert_eq!(probe.0.active.get(), 0);
    }
}

/// STATEMENT: Service call futures retain ordinary task wakers that support remote wakes.
///
/// HOW:
/// - Capture the waker supplied to a pending service call.
/// - Wake it from another thread and verify the caller's wake counter advances.
#[test]
fn service_call_task_wakers_still_support_remote_wakes() {
    let captured = Rc::new(RefCell::new(None));
    let slot = captured.clone();
    let service = crate::fn_service(move |(): ()| {
        let slot = slot.clone();
        async move {
            poll_fn(|cx| {
                *slot.borrow_mut() = Some(cx.waker().clone());
                Poll::<Result<(), ()>>::Pending
            })
            .await
        }
    });
    let pl = Pipeline::new((), service);
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let mut call = Box::pin(pl.call(()));
    assert!(
        call.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let remote = captured.borrow_mut().take().unwrap();
    std::thread::spawn(move || remote.wake()).join().unwrap();
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
}

/// STATEMENT: Pipeline callers share one checker while keeping independent cancellation-aware observers.
///
/// HOW:
/// - Register borrowed, bound, and retained waiters with distinct counters and cancel one.
/// - Notify and cancel the owner, then verify only live observers wake and a single successor completes.
#[test]
fn pipeline_callers_cancel_and_restart_one_checker_with_independent_observers() {
    let probe = Probe::new();
    probe.0.available.set(false);
    let pl = Pipeline::new(State(0), probe.clone());
    let binding = pl.bind();
    let mut first = Box::pin(pl.ready());
    let mut second = Box::pin(pl.ready());
    let mut cancelled = Box::pin(binding.ready());
    let counters = std::array::from_fn::<_, 4, _>(|_| Arc::new(Counter::default()));
    let wakers = counters
        .each_ref()
        .map(|counter| Waker::from(counter.clone()));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[0]))
            .is_pending()
    );
    assert!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[1]))
            .is_pending()
    );
    assert!(
        cancelled
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[2]))
            .is_pending()
    );
    assert!(
        pl.poll_ready(&mut Context::from_waker(&wakers[3]))
            .is_pending()
    );
    assert_eq!(probe.0.starts.get(), 1);
    drop(cancelled);
    assert_eq!(probe.0.drops.get(), 0);
    probe.0.changed.notify();
    probe.0.changed.notify();
    assert_eq!(
        probe.0.drops.get(),
        0,
        "notifications only invalidate and wake"
    );
    assert!(counters[0].0.load(Ordering::Relaxed) > 0);
    assert!(counters[1].0.load(Ordering::Relaxed) > 0);
    assert_eq!(counters[2].0.load(Ordering::Relaxed), 0);
    assert!(counters[3].0.load(Ordering::Relaxed) > 0);
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[0]))
            .is_pending()
    );
    assert_eq!(probe.0.starts.get(), 2);
    assert_eq!(probe.0.drops.get(), 1);
    assert!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[1]))
            .is_pending()
    );
    let before = counters[1].0.load(Ordering::Relaxed);
    drop(first);
    assert_eq!(probe.0.active.get(), 0);
    assert!(counters[1].0.load(Ordering::Relaxed) > before);
    assert!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[1]))
            .is_pending()
    );
    assert_eq!(probe.0.starts.get(), 3);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(
        second.as_mut().poll(&mut Context::from_waker(&wakers[1])),
        Poll::Ready(Ok(()))
    );
    assert_eq!(
        pl.poll_ready(&mut Context::from_waker(&wakers[3])),
        Poll::Ready(Ok(()))
    );
    assert_eq!(probe.0.starts.get(), 4);
    assert_eq!(probe.0.max_active.get(), 1);
}

/// STATEMENT: A retained pipeline checker survives its original poller and a joining future being dropped.
///
/// HOW:
/// - Start retained readiness, cancel a joiner, and drop the original pipeline handle.
/// - Continue through a binding's owned call and verify notification-driven restarts never overlap.
#[test]
fn retained_pipeline_check_survives_poller_and_joiner_loss() {
    let probe = Probe::new();
    probe.0.available.set(false);
    let pl = Pipeline::new(State(10), probe.clone());
    let binding = pl.bind();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(pl.poll_ready(&mut cx).is_pending());
    let mut abandoned = Box::pin(binding.ready());
    assert!(abandoned.as_mut().poll(&mut cx).is_pending());
    drop(abandoned);
    drop(pl);
    assert_eq!(probe.0.drops.get(), 0);
    let mut call = Box::pin(binding.call_static(1));
    assert!(call.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 1);
    probe.0.changed.notify();
    assert_eq!(probe.0.drops.get(), 0);
    assert!(call.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 2);
    assert_eq!(probe.0.drops.get(), 1);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(call.as_mut().poll(&mut cx), Poll::Ready(Ok(11)));
    assert_eq!(probe.0.starts.get(), 3);
    assert_eq!(probe.0.active.get(), 0);
    assert_eq!(probe.0.max_active.get(), 1);
}

/// STATEMENT: A retained checker wakes all live local observers without invalidating readiness.
///
/// HOW:
/// - Register retained and borrowed observers with separate counters, then drop one waiter.
/// - Wake the checker and verify only live counters advance and completion reuses the original check.
#[test]
fn retained_checker_wakes_every_live_observer_on_its_owning_thread() {
    let probe = Probe::new();
    probe.0.available.set(false);
    let pl = Pipeline::new(State(0), probe.clone());
    let mut first = Box::pin(pl.ready());
    let mut second = Box::pin(pl.ready());
    let counters = std::array::from_fn::<_, 3, _>(|_| Arc::new(Counter::default()));
    let wakers = counters
        .each_ref()
        .map(|counter| Waker::from(counter.clone()));
    assert!(
        pl.poll_ready(&mut Context::from_waker(&wakers[0]))
            .is_pending()
    );
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[1]))
            .is_pending()
    );
    assert!(
        second
            .as_mut()
            .poll(&mut Context::from_waker(&wakers[2]))
            .is_pending()
    );
    // The last poller may stop polling without dropping the pipeline.
    assert!(
        pl.poll_ready(&mut Context::from_waker(&wakers[0]))
            .is_pending()
    );
    drop(second);
    let wake = probe.0.waker.borrow_mut().take().unwrap();
    wake.wake();
    assert!(counters[0].0.load(Ordering::Relaxed) > 0);
    assert!(counters[1].0.load(Ordering::Relaxed) > 0);
    assert_eq!(counters[2].0.load(Ordering::Relaxed), 0);
    probe.0.available.set(true);
    assert_eq!(
        first.as_mut().poll(&mut Context::from_waker(&wakers[1])),
        Poll::Ready(Ok(()))
    );
    assert_eq!(
        probe.0.starts.get(),
        1,
        "scheduling wakes do not invalidate"
    );
    assert_eq!(probe.0.max_active.get(), 1);
}

/// STATEMENT: Pipelines reject readiness invalidated during completion and never cache readiness errors.
///
/// HOW:
/// - Invalidate during readiness polling and future destruction, then inject repeated readiness errors.
/// - Verify recheck counts, no erroneous dispatch, and recovery without another notification.
#[test]
fn pipeline_rejects_stale_success_and_does_not_cache_errors() {
    let probe = Probe::new();
    let pl = Pipeline::new(State(0), probe.clone());
    probe.0.invalidate_ready.set(true);
    assert_eq!(run(pl.call(1)), Ok(1));
    assert_eq!(probe.0.starts.get(), 2);
    probe.0.changed.notify();
    probe.0.invalidate_drop.set(true);
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(pl.poll_ready(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(probe.0.starts.get(), 4);
    probe.0.fail.set(true);
    probe.0.changed.notify();
    assert_eq!(pl.poll_ready(&mut cx), Poll::Ready(Err(Error)));
    assert_eq!(run(pl.call(2)), Err(Error));
    assert_eq!(run(pl.call_static(3)), Err(Error));
    assert_eq!(probe.0.calls.get(), 1);
    assert_eq!(probe.0.starts.get(), 7);
    probe.0.fail.set(false);
    assert_eq!(run(pl.call(4)), Ok(4));
    assert_eq!(probe.0.starts.get(), 8);
}

/// STATEMENT: Invalidation during pipeline ownership handoff prevents dispatch on stale readiness.
///
/// HOW:
/// - Park a call and retained observer, then make the handoff wake inject a readiness failure.
/// - Complete the original check and verify a new check returns the error without calling the service.
#[test]
fn pipeline_handoff_invalidation_prevents_stale_dispatch() {
    let probe = Probe::new();
    probe.0.available.set(false);
    let pl = Pipeline::new(State(0), probe.clone());
    let mut call = Box::pin(pl.call(1));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(call.as_mut().poll(&mut cx).is_pending());
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter);
    assert!(pl.poll_ready(&mut Context::from_waker(&waker)).is_pending());
    let changed = probe.clone();
    ON_WAKE.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            changed.0.fail.set(true);
            changed.0.changed.notify();
        }));
    });
    // Complete the pending operation; ownership handoff then invalidates it.
    probe.0.available.set(true);
    assert_eq!(call.as_mut().poll(&mut cx), Poll::Ready(Err(Error)));
    assert_eq!(probe.0.starts.get(), 2);
    assert_eq!(probe.0.calls.get(), 0);
    assert_eq!(probe.0.max_active.get(), 1);
}

/// STATEMENT: `PipelineState` aliases share caches and retained check state without replacing each call's state.
///
/// HOW:
/// - Mix state-taking and fixed-state calls, then start retained readiness with another state.
/// - Drop the original pipeline and complete an owned call, checking readiness-state history and responses.
#[test]
fn state_pipeline_aliases_share_cache_and_retained_checker_but_not_call_state() {
    let probe = Probe::new();
    let pl = PipelineState::new(probe.clone());
    let binding = pl.bind();
    let bound = pl.bind_state(State(20));
    assert_eq!(run(pl.ready(&State(10))), Ok(()));
    assert_eq!(run(binding.call(1, &State(30))), Ok(31));
    assert_eq!(run(bound.ready()), Ok(()));
    assert_eq!(run(bound.call(2)), Ok(22));
    assert_eq!(run(pl.call(3, &State(40))), Ok(43));
    assert_eq!(&*probe.0.states.borrow(), &[10]);

    let mut owned = Box::pin(bound.call_static(4));
    probe.0.available.set(false);
    probe.0.changed.notify();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(pl.poll_ready(&mut cx, &State(50)).is_pending());
    assert!(pl.poll_ready(&mut cx, &State(60)).is_pending());
    assert!(owned.as_mut().poll(&mut cx).is_pending());
    assert_eq!(&*probe.0.states.borrow(), &[10, 50]);
    drop(pl);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(owned.as_mut().poll(&mut cx), Poll::Ready(Ok(24)));
    assert_eq!(&*probe.0.states.borrow(), &[10, 50, 50]);
    assert_eq!(run(binding.call(5, &State(70))), Ok(75));
    assert_eq!(probe.0.max_active.get(), 1);
    assert_eq!(probe.0.starts.get(), 3);
}

/// STATEMENT: Cancelling state-taking readiness hands ownership to the waiting call with its own state.
///
/// HOW:
/// - Park an owner and fixed-state call, cancel the owner, and inspect the successor's state.
/// - Fail its readiness, recover through a third state, and verify no overlapping checks or failed dispatch.
#[test]
fn state_pipeline_cancellation_hands_off_to_the_waiting_calls_state() {
    let probe = Probe::new();
    probe.0.available.set(false);
    let pl = PipelineState::new(probe.clone());
    let bound = pl.bind_state(State(20));
    let mut owner = Box::pin(pl.ready(&State(10)));
    let mut waiting = Box::pin(bound.call_static(1));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(owner.as_mut().poll(&mut cx).is_pending());
    assert!(waiting.as_mut().poll(&mut cx).is_pending());
    assert_eq!(&*probe.0.states.borrow(), &[10]);
    drop(owner);
    assert_eq!(probe.0.active.get(), 0);
    assert!(waiting.as_mut().poll(&mut cx).is_pending());
    assert_eq!(&*probe.0.states.borrow(), &[10, 20]);
    probe.0.available.set(true);
    probe.0.fail.set(true);
    probe.0.changed.notify();
    assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Ready(Err(Error)));
    assert_eq!(probe.0.calls.get(), 0);
    probe.0.fail.set(false);
    assert_eq!(run(pl.call(2, &State(30))), Ok(32));
    assert_eq!(&*probe.0.states.borrow(), &[10, 20, 20, 30]);
    assert_eq!(probe.0.max_active.get(), 1);
}

/// STATEMENT: Shutdown cancels retained readiness without letting bound calls bypass their own checks.
///
/// HOW:
/// - Park retained, pipeline, and binding readiness, then shut down the pipeline.
/// - Verify pipeline waits finish, bound readiness starts anew, and a later bound call checks again.
#[test]
fn shutdown_cancels_retained_checks_without_bypassing_bound_call_readiness() {
    let probe = Probe::new();
    probe.0.available.set(false);
    let pl = Pipeline::new(State(10), probe.clone());
    let binding = pl.bind();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(pl.poll_ready(&mut cx).is_pending());
    let mut main = Box::pin(pl.ready());
    let mut bound = Box::pin(binding.ready());
    assert!(main.as_mut().poll(&mut cx).is_pending());
    assert!(bound.as_mut().poll(&mut cx).is_pending());
    run(pl.shutdown());
    assert_eq!(probe.0.active.get(), 0);
    assert_eq!(probe.0.drops.get(), 1);
    assert_eq!(pl.poll_ready(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(main.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    assert!(bound.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 2);
    drop(bound);
    probe.0.available.set(true);
    assert_eq!(run(binding.call(1)), Ok(11));
    assert_eq!(probe.0.starts.get(), 3);
}

/// STATEMENT: Reentrant shutdown defers destruction of a readiness future until its active poll returns.
///
/// HOW:
/// - Trigger shutdown from a wake hook while a readiness poll invalidates itself.
/// - Inspect the live checker inside the hook and verify complete cleanup after the poll yields.
#[test]
fn reentrant_shutdown_defers_dropping_the_currently_polled_checker() {
    let probe = Probe::new();
    probe.0.invalidate_ready.set(true);
    let pl = Rc::new(Pipeline::new(State(10), probe.clone()));
    let shutdown = pl.clone();
    let checked = probe.clone();
    ON_WAKE.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            run(shutdown.shutdown());
            assert_eq!(checked.0.active.get(), 1);
            assert_eq!(checked.0.drops.get(), 0);
        }));
    });
    let waker = Waker::from(Arc::new(Counter::default()));
    let mut cx = Context::from_waker(&waker);
    assert!(pl.poll_ready(&mut cx).is_pending());
    assert_eq!(probe.0.active.get(), 0);
    assert_eq!(probe.0.drops.get(), probe.0.starts.get());
    assert_eq!(probe.0.max_active.get(), 1);
    assert_eq!(pl.poll_ready(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(probe.0.calls.get(), 0);
}

/// STATEMENT: Scheduling-triggered invalidation is rechecked before raw or bound call dispatch.
///
/// HOW:
/// - Register a waiter whose wake hook changes readiness to an error and notifies.
/// - Call each service form and verify a second check returns the error before any call executes.
#[test]
fn scheduling_invalidation_is_observed_before_dispatch() {
    for raw in [false, true] {
        let st = State(0);
        let waiters = WaitersRef::new();
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        let bound = ServiceBinding::new(probe.clone());
        bound.bind(BindCtx::new(waiters.scope()));
        probe.bind(BindCtx::new(waiters.scope()));
        let idx = waiters.insert();
        waiters.get()[idx as usize] = Some(Waker::from(Arc::new(Counter::default())));
        waiters.get_wakers().push(idx);
        let changed = probe.clone();
        ON_WAKE.with_borrow_mut(|slot| {
            *slot = Some(Box::new(move || {
                changed.0.fail.set(true);
                changed.0.changed.notify();
            }));
        });
        let result = if raw {
            run(ctx.call(&probe, 1))
        } else {
            run(ctx.call(&bound, 1))
        };
        assert_eq!(result, Err(Error));
        assert_eq!(probe.0.starts.get(), 2);
        assert_eq!(probe.0.calls.get(), 0);
    }
}

/// STATEMENT: Raw, cached, direct, and legacy-bypass calls preserve their target and supplied state.
///
/// HOW:
/// - Compare raw rechecks with cached binding calls across two parent contexts and states.
/// - Invalidate and exercise legacy bypass plus direct call recovery, checking results and check counts.
#[test]
#[allow(
    deprecated,
    reason = "verify legacy bypass compatibility with cached calls"
)]
fn cached_and_raw_calls_keep_their_target_and_state() {
    let waiters = WaitersRef::new();
    let st = State(10);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let bound = ServiceBinding::new(probe.clone());
    bound.bind(BindCtx::new(waiters.scope()));
    probe.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.call(&probe, 1)), Ok(11));
    assert_eq!(run(ctx.call(&probe, 2)), Ok(12));
    assert_eq!(probe.0.starts.get(), 2);
    assert_eq!(run(ctx.call(&bound, 3)), Ok(13));
    assert_eq!(run(ctx.call(&bound, 4)), Ok(14));
    assert_eq!(run(ctx.ready(&bound)), Ok(()));
    assert_eq!(probe.0.starts.get(), 3);

    let other_waiters = WaitersRef::new();
    let other_st = State(20);
    let other_ctx = Ctx::<(), State>::new(0, &other_waiters, &other_st);
    bound.bind(BindCtx::new(other_waiters.scope()));
    assert_eq!(run(other_ctx.call(&bound, 5)), Ok(25));
    assert_eq!(probe.0.starts.get(), 3);
    assert_eq!(&*probe.0.states.borrow(), &[10, 10, 10]);

    probe.0.available.set(false);
    probe.0.changed.notify();
    assert_eq!(run(ctx.call_nowait(&bound, 6)), Ok(16));
    assert_eq!(probe.0.starts.get(), 3, "legacy bypass does not check");
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(run(bound.call(7, ctx.cast())), Ok(17));
    assert_eq!(probe.0.starts.get(), 4);
}

/// STATEMENT: Raw calls use the closest scope's cache but cannot validate unready siblings.
///
/// HOW:
/// - Validate a bound raw pair, then call one child through the pair's scope.
/// - Invalidate the other child into failure and verify child-only checks never mark the pair ready.
#[test]
fn raw_calls_use_the_closest_scope_without_caching_partial_readiness() {
    struct RawPair(Probe, Probe);

    impl Service<State, usize> for RawPair {
        type Res = usize;
        type Error = Error;

        crate::forward_bind!(State, 0, 1);

        async fn ready(&self, ctx: Ctx<'_, Self, State>) -> Result<(), Error> {
            ctx.ready(&self.0).await?;
            ctx.ready(&self.1).await
        }

        async fn call(&self, req: usize, ctx: Ctx<'_, Self, State>) -> Result<usize, Error> {
            ctx.call(&self.0, req).await
        }
    }

    let waiters = WaitersRef::new();
    let st = State(10);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let a = Probe::new();
    let b = Probe::new();
    let parent = ServiceBinding::new(RawPair(a.clone(), b.clone()));
    parent.bind(BindCtx::new(waiters.scope()));
    let child_ctx = ctx.with_scope::<()>(&parent.scope);
    assert_eq!(run(ctx.call(&parent, 1)), Ok(11));
    assert_eq!(run(child_ctx.call(&a, 2)), Ok(12));
    assert!(!ctx.scope().is_ready());
    assert_eq!((a.0.starts.get(), b.0.starts.get()), (1, 1));

    b.0.fail.set(true);
    b.0.changed.notify();
    assert_eq!(run(child_ctx.call(&a, 3)), Ok(13));
    assert_eq!(run(child_ctx.call(&a, 4)), Ok(14));
    assert!(
        !parent.is_ready(),
        "a raw child cannot validate its sibling"
    );
    assert_eq!((a.0.starts.get(), b.0.starts.get()), (3, 1));
    assert_eq!(run(ctx.call(&parent, 5)), Err(Error));
    assert_eq!((a.0.starts.get(), b.0.starts.get()), (4, 2));
    assert_eq!(a.0.calls.get(), 4);
}

/// STATEMENT: Bound calls require their own readiness even when the enclosing scope is ready.
///
/// HOW:
/// - Call a failing child binding through a ready parent's context and directly.
/// - Recover the child, then notify it and verify cache reuse and retained parent invalidation.
#[test]
fn bound_calls_do_not_borrow_a_ready_outer_scope() {
    let waiters = WaitersRef::new();
    let st = State(10);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let parent = ServiceBinding::new(Probe::new());
    let child_ctx = ctx.with_scope::<()>(&parent.scope);
    let probe = Probe::new();
    probe.0.fail.set(true);
    let bound = ServiceBinding::new(probe.clone());
    parent.bind(BindCtx::new(waiters.scope()));
    bound.bind(BindCtx::new(&parent.scope));
    assert_eq!(run(ctx.ready(&parent)), Ok(()));
    assert_eq!(run(child_ctx.call(&bound, 1)), Err(Error));
    assert_eq!(run(bound.call(2, child_ctx.cast())), Err(Error));
    assert_eq!(probe.0.calls.get(), 0);
    probe.0.fail.set(false);
    assert_eq!(run(bound.call(3, child_ctx.cast())), Ok(13));
    assert_eq!(run(child_ctx.call(&bound, 4)), Ok(14));
    assert_eq!(probe.0.starts.get(), 3);
    probe.0.changed.notify();
    assert!(!parent.is_ready(), "calls retain the parent dependency");
    assert_eq!(run(child_ctx.call(&bound, 5)), Ok(15));
    assert_eq!(probe.0.starts.get(), 4);
}

/// STATEMENT: Legacy nowait traverses transparent bindings but does not bypass nested context calls.
///
/// HOW:
/// - Call a closed service through erased, mapped, and nested bindings without readiness checks.
/// - Add a forwarding context call and verify pending readiness, cancellation cleanup, and error propagation.
#[test]
#[allow(
    deprecated,
    reason = "verify legacy bypass forwarding, errors and cancellation"
)]
fn nowait_bypasses_transparent_bindings_but_not_nested_context_calls() {
    let waiters = WaitersRef::new();
    let st = State(0);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    probe.0.available.set(false);
    let bound = Rc::new(ServiceBinding::new(ServiceBinding::new(probe.clone())));
    let erased = crate::boxed::service(crate::map_state(
        State(10),
        Box::new(bound.clone())
            .map(|res| res + 100)
            .map_err(|err| err),
    ));
    erased.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.call_nowait(&&erased, 1)), Ok(111));
    assert_eq!(probe.0.starts.get(), 0);
    assert!(!bound.is_ready());

    probe.0.call_fail.set(true);
    assert_eq!(run(ctx.call_nowait(&&erased, 2)), Err(Error));
    assert_eq!(probe.0.starts.get(), 0, "only the call error is returned");
    probe.0.call_fail.set(false);

    let parent = ServiceBinding::new(Forward(bound));
    parent.bind(BindCtx::new(waiters.scope()));
    let mut call = Box::pin(ctx.call_nowait(&parent, 2));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(call.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 1);
    assert_eq!(probe.0.calls.get(), 2);
    drop(call);
    assert_eq!(probe.0.active.get(), 0);
    assert_eq!(probe.0.drops.get(), 1);

    let mut call = Box::pin(ctx.call_nowait(&parent, 3));
    assert!(call.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 2);
    probe.0.available.set(true);
    probe.0.fail.set(true);
    probe.0.changed.notify();
    assert_eq!(call.as_mut().poll(&mut cx), Poll::Ready(Err(Error)));
    assert_eq!(probe.0.calls.get(), 2);
}

struct Forward<S>(S);

impl<S: Service<State, usize>> Service<State, usize> for Forward<S> {
    type Res = S::Res;
    type Error = S::Error;

    crate::forward_bind!(State, self.0);

    async fn ready(&self, ctx: Ctx<'_, Self, State>) -> Result<(), S::Error> {
        ctx.ready(&self.0).await
    }

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, State>) -> Result<S::Res, S::Error> {
        ctx.call(&self.0, req).await
    }
}

/// STATEMENT: Binding clones get fresh caches while shared aliases retain cache-hit parent dependencies.
///
/// HOW:
/// - Compare a direct binding clone with reference-counted aliases beneath two parents.
/// - Notify the common service and verify all parents invalidate while aliases share one recheck.
#[test]
fn fresh_clones_and_shared_aliases_retain_cache_hit_dependencies() {
    let waiters = WaitersRef::new();
    let st = State(0);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let bound = ServiceBinding::new(probe.clone());
    bound.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&bound)), Ok(()));
    let fresh = bound.clone();
    fresh.bind(BindCtx::new(waiters.scope()));
    assert!(!fresh.scope.is_ready());
    assert_eq!(run(ctx.ready(&fresh)), Ok(()));
    assert_eq!(probe.0.starts.get(), 2);

    let shared = Rc::new(bound);
    let alias = shared.clone();
    assert_eq!(run(ctx.ready(&alias)), Ok(()));
    let left = ServiceBinding::new(Forward(shared));
    let right = ServiceBinding::new(Forward(alias));
    left.bind(BindCtx::new(waiters.scope()));
    right.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&left)), Ok(()));
    assert_eq!(run(ctx.ready(&right)), Ok(()));
    assert_eq!(probe.0.starts.get(), 2);
    probe.0.changed.notify();
    assert!(!left.scope.is_ready());
    assert!(!right.scope.is_ready());
    assert!(!fresh.scope.is_ready());
    assert_eq!(run(ctx.ready(&left)), Ok(()));
    assert_eq!(run(ctx.ready(&right)), Ok(()));
    assert_eq!(probe.0.starts.get(), 3);
    assert_eq!(run(ctx.ready(&fresh)), Ok(()));
    assert_eq!(probe.0.starts.get(), 4);
}

/// STATEMENT: Shared binding readiness retains one checker across repolls, invalidation, and owner handoff.
///
/// HOW:
/// - Join three parent contexts, cancel a waiter, notify twice, and cancel the owner.
/// - Check wake delivery, drop/start counts, unchanged handoff generation, and single-checker completion.
#[test]
fn shared_checker_retains_restarts_and_hands_off_pending_work() {
    let st = State(0);
    let waiters = WaitersRef::new();
    let other_waiters = WaitersRef::new();
    let cancelled_waiters = WaitersRef::new();
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let other_ctx = Ctx::<(), State>::new(0, &other_waiters, &st);
    let cancelled_ctx = Ctx::<(), State>::new(0, &cancelled_waiters, &st);
    let probe = Probe::new();
    probe.0.available.set(false);
    let bound = ServiceBinding::new(probe.clone());
    bound.bind(BindCtx::new(waiters.scope()));
    bound.bind(BindCtx::new(other_waiters.scope()));
    bound.bind(BindCtx::new(cancelled_waiters.scope()));
    let mut first = Box::pin(ctx.ready(&bound));
    let mut second = Box::pin(other_ctx.ready(&bound));
    let mut cancelled = Box::pin(cancelled_ctx.ready(&bound));
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert!(cancelled.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 1);
    assert_eq!(probe.0.active.get(), 1);
    let generation = bound.scope.generation();
    drop(cancelled);
    assert_eq!(probe.0.drops.get(), 0);
    assert!(bound.scope.is_current(generation));

    probe.0.changed.notify();
    probe.0.changed.notify();
    assert_eq!(probe.0.drops.get(), 0, "notify must not drop the check");
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 2);
    assert_eq!(probe.0.drops.get(), 1);
    assert_eq!(probe.0.active.get(), 1);
    assert!(second.as_mut().poll(&mut cx).is_pending());
    let before = counter.0.load(Ordering::Relaxed);
    let generation = bound.scope.generation();
    drop(first);
    assert_eq!(probe.0.active.get(), 0);
    assert_eq!(probe.0.drops.get(), 2);
    assert!(counter.0.load(Ordering::Relaxed) > before);
    assert!(
        bound.scope.is_current(generation),
        "handoff is not invalidation"
    );
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 3);
    assert_eq!(probe.0.active.get(), 1);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(second.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(probe.0.starts.get(), 4);
    assert_eq!(probe.0.active.get(), 0);
    assert_eq!(probe.0.max_active.get(), 1);
    assert!(bound.scope.is_ready());
}

/// STATEMENT: Raw pending readiness reacts to notifications without caching completed success.
///
/// HOW:
/// - Notify a pending raw check and verify it wakes and restarts only when polled.
/// - Complete it, check again, and verify a fresh invocation with no overlapping checks.
#[test]
fn raw_pending_checks_observe_notifications_without_caching() {
    let st = State(0);
    let waiters = WaitersRef::new();
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    probe.0.available.set(false);
    probe.bind(BindCtx::new(waiters.scope()));
    let mut ready = Box::pin(ctx.ready(&probe));
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(ready.as_mut().poll(&mut cx).is_pending());
    probe.0.changed.notify();
    assert!(counter.0.load(Ordering::Relaxed) > 0);
    assert_eq!(probe.0.drops.get(), 0);
    assert!(ready.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 2);
    assert_eq!(probe.0.drops.get(), 1);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(run(ctx.ready(&probe)), Ok(()));
    assert_eq!(probe.0.starts.get(), 4);
    assert_eq!(probe.0.max_active.get(), 1);
}

struct Pair {
    a: ServiceBinding<Probe>,
    b: ServiceBinding<Probe>,
    starts: Cell<usize>,
}

impl Service<State, usize> for Pair {
    type Res = usize;
    type Error = Error;

    crate::forward_bind!(State, a, b);

    async fn ready(&self, ctx: Ctx<'_, Self, State>) -> Result<(), Error> {
        self.starts.set(self.starts.get() + 1);
        ctx.ready(&self.a).await?;
        ctx.ready(&self.b).await
    }

    async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Error> {
        Ok(req)
    }
}

/// STATEMENT: Pipeline ancestor invalidation preserves a pending sibling until aggregate revalidation.
///
/// HOW:
/// - Start sequential child readiness and invalidate the already-completed child.
/// - Complete the pending sibling, then reopen the first and check selective restart and call success.
#[test]
fn pipeline_ancestor_invalidation_preserves_pending_siblings() {
    let a = Probe::new();
    let b = Probe::new();
    b.0.available.set(false);
    let pair = Rc::new(Pair {
        a: ServiceBinding::new(a.clone()),
        b: ServiceBinding::new(b.clone()),
        starts: Cell::new(0),
    });
    let pl = Pipeline::new(State(0), pair.clone());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(pl.poll_ready(&mut cx).is_pending());
    a.0.available.set(false);
    a.0.changed.notify();
    assert!(pl.poll_ready(&mut cx).is_pending());
    assert_eq!(pair.starts.get(), 1);
    assert_eq!(b.0.starts.get(), 1);
    assert_eq!(b.0.drops.get(), 0);
    b.0.available.set(true);
    b.0.changed.notify();
    assert!(pl.poll_ready(&mut cx).is_pending());
    assert_eq!(pair.starts.get(), 2);
    assert_eq!(a.0.starts.get(), 2);
    assert_eq!(b.0.starts.get(), 2);
    a.0.available.set(true);
    a.0.changed.notify();
    assert_eq!(pl.poll_ready(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(run(pl.call(1)), Ok(1));
    assert_eq!(a.0.starts.get(), 3);
    assert_eq!(b.0.starts.get(), 2);
}

/// STATEMENT: Bound ancestor invalidation preserves pending siblings but rejects stale aggregate success.
///
/// HOW:
/// - Invalidate the completed child while a sequential sibling remains pending.
/// - Complete each child in turn and verify the clean sibling stays cached during parent revalidation.
#[test]
fn ancestor_invalidation_preserves_pending_siblings_and_rechecks_success() {
    let st = State(0);
    let waiters = WaitersRef::new();
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let a = Probe::new();
    let b = Probe::new();
    b.0.available.set(false);
    let parent = ServiceBinding::new(Pair {
        a: ServiceBinding::new(a.clone()),
        b: ServiceBinding::new(b.clone()),
        starts: Cell::new(0),
    });
    parent.bind(BindCtx::new(waiters.scope()));
    let mut ready = Box::pin(ctx.ready(&parent));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(ready.as_mut().poll(&mut cx).is_pending());
    assert_eq!(a.0.starts.get(), 1);
    assert_eq!(b.0.starts.get(), 1);
    a.0.available.set(false);
    a.0.changed.notify();
    assert!(ready.as_mut().poll(&mut cx).is_pending());
    assert_eq!(parent.service.starts.get(), 1);
    assert_eq!(b.0.starts.get(), 1);
    assert_eq!(b.0.drops.get(), 0);

    b.0.available.set(true);
    b.0.changed.notify();
    assert!(ready.as_mut().poll(&mut cx).is_pending());
    assert!(!parent.scope.is_ready());
    assert_eq!(a.0.starts.get(), 2);
    assert_eq!(b.0.starts.get(), 2);
    assert!(parent.service.b.scope.is_ready());
    a.0.available.set(true);
    a.0.changed.notify();
    assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    assert_eq!(a.0.starts.get(), 3);
    assert_eq!(b.0.starts.get(), 2, "the clean sibling stays cached");
    assert!(parent.scope.is_ready());
}

/// STATEMENT: Binding caches reject stale successes and never retain readiness errors.
///
/// HOW:
/// - Invalidate during readiness polling and future destruction, then inject errors into calls and checks.
/// - Verify rechecks, an unready cache, no failed dispatch, and recovery without another notification.
#[test]
fn stale_success_and_errors_are_not_cached() {
    let st = State(0);
    let waiters = WaitersRef::new();
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    probe.0.invalidate_ready.set(true);
    let bound = ServiceBinding::new(probe.clone());
    bound.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&bound)), Ok(()));
    assert_eq!(probe.0.starts.get(), 2);

    probe.0.changed.notify();
    probe.0.invalidate_drop.set(true);
    assert_eq!(run(ctx.ready(&bound)), Ok(()));
    assert_eq!(probe.0.starts.get(), 4);
    probe.0.fail.set(true);
    probe.0.changed.notify();
    assert_eq!(run(ctx.call(&bound, 1)), Err(Error));
    assert_eq!(run(ctx.ready(&bound)), Err(Error));
    assert_eq!(probe.0.starts.get(), 6);
    assert_eq!(probe.0.calls.get(), 0);
    assert!(!bound.scope.is_ready());
    probe.0.fail.set(false);
    assert_eq!(run(ctx.call(&bound, 1)), Ok(1));
    assert_eq!(probe.0.starts.get(), 7);
}

/// STATEMENT: A notification emitted during a call gates the next call on fresh readiness.
///
/// HOW:
/// - Make the first call close and invalidate its own binding.
/// - Verify the next call stays pending until the service is reopened and notified.
#[test]
fn call_notifications_gate_the_next_call() {
    let st = State(0);
    let waiters = WaitersRef::new();
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let bound = ServiceBinding::new(probe.clone());
    bound.bind(BindCtx::new(waiters.scope()));
    probe.0.invalidate_call.set(true);
    assert_eq!(run(ctx.call(&bound, 1)), Ok(1));
    assert!(!bound.scope.is_ready());
    let mut next = Box::pin(ctx.call(&bound, 2));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(next.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.calls.get(), 1);
    probe.0.available.set(true);
    probe.0.changed.notify();
    assert_eq!(next.as_mut().poll(&mut cx), Poll::Ready(Ok(2)));
    assert_eq!(probe.0.calls.get(), 2);
}

/// STATEMENT: Transparent adapters preserve shared readiness, mapped state, and lifecycle dependencies.
///
/// HOW:
/// - Share a binding through boxed, erased, response/error-mapped, and state-mapped parents.
/// - Check responses, selective rechecks, shutdown propagation, and fresh caches for cloned bindings.
#[test]
fn transparent_adapters_preserve_shared_readiness_and_mapped_state() {
    let waiters = WaitersRef::new();
    let ctx = Ctx::<(), ()>::new(0, &waiters, &());
    let probe = Probe::new();
    let bound = Rc::new(ServiceBinding::new(probe.clone()));
    let mapped = crate::map_state(
        State(10),
        Box::new(bound.clone())
            .map(|res| res + 100)
            .map_err(|_: Error| "unavailable"),
    );
    let erased: crate::boxed::BoxService<(), usize, usize, &str> = crate::boxed::service(mapped);
    let alias = erased.clone();
    erased.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.call(&&erased, 1)), Ok(111));
    let left = ServiceBinding::new(alias);
    let right = ServiceBinding::new(crate::map_state(State(20), bound.clone()));
    left.bind(BindCtx::new(waiters.scope()));
    Service::<(), usize>::bind(&right, BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.call(&left, 2)), Ok(112));
    assert_eq!(run(ctx.call(&right, 3)), Ok(23));
    assert_eq!(&*probe.0.states.borrow(), &[10]);

    probe.0.call_changed.notify();
    assert!(!left.scope.is_ready());
    assert!(!right.scope.is_ready());
    assert_eq!(run(ctx.call(&right, 4)), Ok(24));
    assert_eq!(run(ctx.call(&left, 5)), Ok(115));
    assert_eq!(&*probe.0.states.borrow(), &[10, 20]);
    run(ctx.shutdown(&left));
    assert_eq!(probe.0.shutdown_state.get(), 10);
    probe.0.shutdown_changed.notify();
    assert!(!bound.scope.is_ready());
    assert!(!left.scope.is_ready());
    assert!(!right.scope.is_ready());

    let mapped = crate::map_state(State(30), ServiceBinding::new(probe.clone()));
    let fresh = mapped.clone();
    Service::<(), usize>::bind(&mapped, BindCtx::new(waiters.scope()));
    Service::<(), usize>::bind(&fresh, BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&mapped)), Ok(()));
    assert_eq!(run(ctx.ready(&fresh)), Ok(()));
    assert_eq!(&*probe.0.states.borrow(), &[10, 20, 30, 30]);
}

/// STATEMENT: Transparent raw adapters add no cache or redundant checks after pipeline validation.
///
/// HOW:
/// - Call an erased, mapped raw service directly and verify one check per call.
/// - Put it in a pipeline and verify one shared check until the underlying notifier invalidates it.
#[test]
fn transparent_raw_adapters_do_not_cache_or_repeat_validated_checks() {
    let waiters = WaitersRef::new();
    let ctx = Ctx::<(), ()>::new(0, &waiters, &());
    let probe = Probe::new();
    let mapped = crate::map_state(
        State(42),
        Box::new(Rc::new(probe.clone()))
            .map(|res| res + 100)
            .map_err(|_: Error| "unavailable"),
    );
    let erased: crate::boxed::BoxService<(), usize, usize, &str> = crate::boxed::service(mapped);
    erased.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.call(&&erased, 1)), Ok(143));
    assert_eq!(run(ctx.call(&erased.clone(), 2)), Ok(144));
    assert_eq!(probe.0.starts.get(), 2);
    let pl = Pipeline::new((), erased);
    assert_eq!(run(pl.call(3)), Ok(145));
    assert_eq!(run(pl.call(4)), Ok(146));
    assert_eq!(probe.0.starts.get(), 3);
    probe.0.changed.notify();
    assert_eq!(run(pl.call(5)), Ok(147));
    assert_eq!(probe.0.starts.get(), 4);
}

/// STATEMENT: Erased aliases hand off cancelled readiness while preserving mapped state and errors.
///
/// HOW:
/// - Join a readiness owner and call through aliased erased services, invalidate, then cancel the owner.
/// - Verify wake and handoff, mapped failure without dispatch, and shared-cache recovery.
#[test]
fn erased_aliases_handoff_cancelled_checks_and_preserve_mapped_errors() {
    let waiters = WaitersRef::new();
    let other_waiters = WaitersRef::new();
    let ctx = Ctx::<(), ()>::new(0, &waiters, &());
    let other_ctx = Ctx::<(), ()>::new(0, &other_waiters, &());
    let probe = Probe::new();
    probe.0.available.set(false);
    let erased: crate::boxed::BoxService<(), usize, usize, &str> =
        crate::boxed::service(crate::map_state(
            State(42),
            ServiceBinding::new(probe.clone()).map_err(|_: Error| "unavailable"),
        ));
    let alias = erased.clone();
    erased.bind(BindCtx::new(waiters.scope()));
    alias.bind(BindCtx::new(other_waiters.scope()));
    let mut first = Box::pin(ctx.ready(&erased));
    let mut second = Box::pin(other_ctx.call(&alias, 1));
    let counter = Arc::new(Counter::default());
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 1);
    probe.0.changed.notify();
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 2);
    let before = counter.0.load(Ordering::Relaxed);
    drop(first);
    assert_eq!(probe.0.active.get(), 0);
    assert!(counter.0.load(Ordering::Relaxed) > before);
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert_eq!(probe.0.starts.get(), 3);
    probe.0.available.set(true);
    probe.0.fail.set(true);
    probe.0.changed.notify();
    assert_eq!(
        second.as_mut().poll(&mut cx),
        Poll::Ready(Err("unavailable"))
    );
    assert_eq!(probe.0.calls.get(), 0);
    probe.0.fail.set(false);
    assert_eq!(run(ctx.call(&erased, 2)), Ok(44));
    assert_eq!(run(other_ctx.call(&alias, 3)), Ok(45));
    assert_eq!(probe.0.starts.get(), 5);
    assert_eq!(probe.0.max_active.get(), 1);
    assert!(probe.0.states.borrow().iter().all(|st| *st == 42));
}

struct PipelineForward {
    inner: Rc<Pipeline<usize, usize, Error>>,
}

impl Service<State, usize> for PipelineForward {
    type Res = usize;
    type Error = Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        self.inner.bind_to(&ctx);
    }

    crate::forward_pl_ready!(State, self.inner);
    crate::forward_pl_shutdown!(State, self.inner);

    async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Error> {
        self.inner.call(req).await
    }
}

/// STATEMENT: Forwarded pipeline readiness and shutdown invalidate every linked ancestor.
///
/// HOW:
/// - Link a previously ready pipeline beneath nested bindings and notify its lifecycle sources.
/// - Verify cache reuse, ancestor invalidation before shutdown wakes, and forwarded shutdown state.
#[test]
fn pipeline_forwarding_links_cached_children_and_shutdown_to_all_ancestors() {
    let waiters = WaitersRef::new();
    let st = State(0);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let pl = Rc::new(Pipeline::new(State(10), probe.clone()));
    assert_eq!(run(pl.ready()), Ok(()));
    let parent = Rc::new(ServiceBinding::new(PipelineForward { inner: pl.clone() }));
    let ancestor = Rc::new(ServiceBinding::new(parent.clone()));
    ancestor.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.call(&ancestor, 1)), Ok(11));
    assert_eq!(probe.0.starts.get(), 1);
    probe.0.changed.notify();
    assert!(!parent.scope.is_ready());
    assert!(!ancestor.scope.is_ready());
    assert_eq!(run(ctx.ready(&ancestor)), Ok(()));
    assert_eq!(probe.0.starts.get(), 2);
    probe.0.call_changed.notify();
    assert!(!ancestor.scope.is_ready());
    assert_eq!(run(ctx.ready(&ancestor)), Ok(()));
    assert_eq!(probe.0.starts.get(), 3);

    let observer = parent.scope.observer();
    let counter = Arc::new(Counter::default());
    observer.register(&Waker::from(counter.clone()));
    let checked_parent = parent.clone();
    let checked_ancestor = ancestor.clone();
    ON_WAKE.with_borrow_mut(|slot| {
        *slot = Some(Box::new(move || {
            assert!(!checked_parent.scope.is_ready());
            assert!(!checked_ancestor.scope.is_ready());
        }));
    });
    run(pl.shutdown());
    assert!(counter.0.load(Ordering::Relaxed) > 0);
    assert!(!parent.scope.is_ready());
    assert!(!ancestor.scope.is_ready());
    assert_eq!(run(ctx.ready(&ancestor)), Ok(()));
    probe.0.shutdown_changed.notify();
    assert!(!ancestor.scope.is_ready());
    assert_eq!(probe.0.shutdown_state.get(), 10);
}

struct BindingForward {
    inner: crate::pipeline::PipelineBinding<usize, usize, Error>,
}

impl Service<State, usize> for BindingForward {
    type Res = usize;
    type Error = &'static str;

    crate::forward_pl_bind!(State, self.inner);
    crate::forward_pl_ready!(State, self.inner, |_| "unavailable");

    async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Self::Error> {
        self.inner.call(req).await.map_err(|_| "unavailable")
    }
}

/// STATEMENT: Forwarded fixed-state bindings share readiness while preserving distinct call states.
///
/// HOW:
/// - Call two forwarded bindings and a state-taking handle after one shared readiness check.
/// - Inject mapped readiness errors, recover, and verify shutdown invalidates both parents.
#[test]
fn state_binding_forwarding_shares_readiness_without_losing_call_state() {
    let waiters = WaitersRef::new();
    let st = State(0);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let pl = PipelineState::new(probe.clone());
    let left = ServiceBinding::new(BindingForward {
        inner: pl.bind_state(State(10)),
    });
    let right = ServiceBinding::new(BindingForward {
        inner: pl.bind_state(State(20)),
    });
    left.bind(BindCtx::new(waiters.scope()));
    right.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(pl.ready(&State(30))), Ok(()));
    assert_eq!(run(ctx.call(&left, 1)), Ok(11));
    assert_eq!(run(ctx.call(&right, 2)), Ok(22));
    assert_eq!(run(pl.bind().call(3, &State(40))), Ok(43));
    assert_eq!(&*probe.0.states.borrow(), &[30]);
    probe.0.fail.set(true);
    probe.0.changed.notify();
    assert!(!left.scope.is_ready());
    assert!(!right.scope.is_ready());
    assert_eq!(run(ctx.call(&left, 4)), Err("unavailable"));
    assert_eq!(probe.0.calls.get(), 3);
    probe.0.fail.set(false);
    assert_eq!(run(ctx.call(&right, 5)), Ok(25));
    assert_eq!(run(ctx.ready(&left)), Ok(()));
    assert_eq!(&*probe.0.states.borrow(), &[30, 10, 20]);
    run(pl.shutdown(&State(50)));
    assert!(!left.scope.is_ready());
    assert!(!right.scope.is_ready());
    assert_eq!(probe.0.shutdown_state.get(), 50);
    assert_eq!(run(ctx.call(&left, 6)), Ok(16));
    assert_eq!(run(ctx.ready(&right)), Ok(()));
    probe.0.shutdown_changed.notify();
    assert!(!left.scope.is_ready());
    assert!(!right.scope.is_ready());
}

struct ShutdownForward {
    inner: Pipeline<usize, usize, Error>,
}

impl Service<State, usize> for ShutdownForward {
    type Res = usize;
    type Error = Error;

    crate::forward_pl_bind!(State, self.inner);
    crate::forward_pl_shutdown!(State, self.inner);

    async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Error> {
        self.inner.call(req).await
    }
}

/// STATEMENT: Shutdown forwarding uses dependencies established before inner readiness is ever checked.
///
/// HOW:
/// - Validate a parent whose readiness does not poll its inner pipeline.
/// - Forward shutdown and notify its source, verifying parent invalidation and the inner shutdown state.
#[test]
fn shutdown_forwarding_uses_dependencies_bound_before_a_readiness_check() {
    let waiters = WaitersRef::new();
    let st = State(0);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let parent = ServiceBinding::new(ShutdownForward {
        inner: Pipeline::new(State(10), probe.clone()),
    });
    parent.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&parent)), Ok(()));
    assert_eq!(probe.0.starts.get(), 0);
    run(ctx.shutdown(&parent));
    assert!(!parent.scope.is_ready());
    assert_eq!(run(ctx.ready(&parent)), Ok(()));
    probe.0.shutdown_changed.notify();
    assert!(!parent.scope.is_ready());
    assert_eq!(probe.0.shutdown_state.get(), 10);
}

/// STATEMENT: Erased binding shutdown notifications reach a ready parent without prior child readiness.
///
/// HOW:
/// - Bind an erased, state-mapped child through a shutdown-only forwarding wrapper.
/// - Shut it down, verify no readiness checks, and notify shutdown to invalidate the parent.
#[test]
fn erased_binding_shutdown_subscriptions_reach_a_previously_ready_parent() {
    struct ShutdownOnly<S> {
        inner: S,
    }

    impl<S: Service<State, usize>> Service<State, usize> for ShutdownOnly<S> {
        type Res = S::Res;
        type Error = S::Error;

        crate::forward_bind!(State, self.inner);
        crate::forward_shutdown!(State, self.inner);

        async fn call(&self, req: usize, ctx: Ctx<'_, Self, State>) -> Result<S::Res, S::Error> {
            ctx.call(&self.inner, req).await
        }
    }

    let waiters = WaitersRef::new();
    let st = State(0);
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let parent = ServiceBinding::new(ShutdownOnly {
        inner: crate::boxed::service(crate::map_state(
            State(42),
            ServiceBinding::new(probe.clone()),
        )),
    });
    parent.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&parent)), Ok(()));
    run(ctx.shutdown(&parent));
    assert_eq!(probe.0.starts.get(), 0);
    assert_eq!(probe.0.shutdown_state.get(), 42);
    probe.0.shutdown_changed.notify();
    assert!(!parent.scope.is_ready());
}

/// STATEMENT: Erased, state-mapped apply contexts preserve subscriptions across every lifecycle stage.
///
/// HOW:
/// - Wrap an apply service in mapped state, type erasure, and a binding.
/// - Exercise readiness, calls, and shutdown, checking mapped state and invalidation from each notifier.
#[test]
fn erased_mapped_and_apply_contexts_preserve_lifecycle_subscriptions() {
    let st = State(1);
    let waiters = WaitersRef::new();
    let ctx = Ctx::<(), State>::new(0, &waiters, &st);
    let probe = Probe::new();
    let applied = crate::apply_fn(
        probe.clone(),
        async |req: usize, ctx: &crate::dev::ApplyCtx<'_, Probe, State, usize>| {
            assert_eq!(ctx.st().0, 42);
            ctx.call(req).await
        },
    );
    let bound = ServiceBinding::new(crate::boxed::service(crate::map_state(State(42), applied)));
    bound.bind(BindCtx::new(waiters.scope()));
    assert_eq!(run(ctx.ready(&bound)), Ok(()));
    probe.0.changed.notify();
    assert!(!bound.scope.is_ready());
    assert_eq!(run(ctx.call(&bound, 1)), Ok(43));
    assert!(probe.0.states.borrow().iter().all(|st| *st == 42));
    probe.0.call_changed.notify();
    assert!(!bound.scope.is_ready());
    assert_eq!(run(ctx.ready(&bound)), Ok(()));
    run(ctx.shutdown(&bound));
    assert_eq!(probe.0.shutdown_state.get(), 42);
    probe.0.shutdown_changed.notify();
    assert!(!bound.scope.is_ready());
}
