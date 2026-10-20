use crate::{Ctx, Service, readiness::ReadyScope};

/// A service paired with persistent readiness.
///
/// [`Ctx::ready`] and [`Ctx::call`] reuse the binding's successful readiness
/// until a subscribed [`crate::ReadyNotifier`] invalidates it. Calls do not
/// consume readiness. Services whose readiness can change must subscribe through
/// [`Ctx::subscribe`] before inspecting availability and notify when it changes.
///
/// A binding owns its service, not pipeline state. Its readiness describes one
/// shared gate regardless of the state passed to a check or call. Independent
/// gates require separate bindings.
///
/// Cloning a binding clones the service into a fresh, initially invalid scope.
/// Use [`std::rc::Rc<ServiceBinding<S>>`] to share a service and its readiness.
#[derive(Debug)]
pub struct ServiceBinding<S> {
    service: S,
    scope: ReadyScope,
}

impl<S> ServiceBinding<S> {
    /// Binds a service to a fresh readiness scope.
    pub fn new(service: S) -> Self {
        Self {
            service,
            scope: ReadyScope::new(),
        }
    }
}

impl<S: Clone> Clone for ServiceBinding<S> {
    fn clone(&self) -> Self {
        Self::new(self.service.clone())
    }
}

impl<S, St, Req> Service<St, Req> for ServiceBinding<S>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.scope().depend_on(&self.scope);
        self.scope
            .check(|| self.service.ready(ctx.with_scope(&self.scope)))
            .await
    }

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        self.service.call(req, ctx.with_scope(&self.scope)).await
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        self.service.shutdown(ctx.with_scope(&self.scope)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::{
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

    use super::*;
    use crate::{ReadyNotifier, ctx::WaitersRef};

    thread_local! {
        static ON_WAKE: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::default();
    }

    struct State(usize);

    #[derive(Debug, PartialEq, Eq)]
    struct Error;

    #[derive(Default)]
    struct ProbeState {
        changed: ReadyNotifier,
        call_changed: ReadyNotifier,
        shutdown_changed: ReadyNotifier,
        available: Cell<bool>,
        fail: Cell<bool>,
        invalidate_call: Cell<bool>,
        invalidate_ready: Cell<bool>,
        invalidate_drop: Cell<bool>,
        starts: Cell<usize>,
        active: Cell<usize>,
        max_active: Cell<usize>,
        drops: Cell<usize>,
        calls: Cell<usize>,
        states: RefCell<Vec<usize>>,
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

        fn ready(&self, ctx: Ctx<'_, Self, State>) -> impl Future<Output = Result<(), Error>> {
            ctx.subscribe(&self.0.changed);
            self.0.starts.set(self.0.starts.get() + 1);
            self.0.active.set(self.0.active.get() + 1);
            self.0
                .max_active
                .set(self.0.max_active.get().max(self.0.active.get()));
            self.0.states.borrow_mut().push(ctx.st().0);
            let check = PendingCheck(&self.0);
            async move {
                let _check = check;
                poll_fn(|_| {
                    if !self.0.available.get() {
                        Poll::Pending
                    } else if self.0.fail.get() {
                        Poll::Ready(Err(Error))
                    } else {
                        if self.0.invalidate_ready.replace(false) {
                            self.0.changed.notify();
                        }
                        Poll::Ready(Ok(()))
                    }
                })
                .await
            }
        }

        async fn call(&self, req: usize, ctx: Ctx<'_, Self, State>) -> Result<usize, Error> {
            ctx.subscribe(&self.0.call_changed);
            self.0.calls.set(self.0.calls.get() + 1);
            if self.0.invalidate_call.replace(false) {
                self.0.available.set(false);
                self.0.call_changed.notify();
            }
            Ok(req + ctx.st().0)
        }

        async fn shutdown(&self, ctx: Ctx<'_, Self, State>) {
            ctx.subscribe(&self.0.shutdown_changed);
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

    #[test]
    fn scheduling_invalidation_is_observed_before_dispatch() {
        let st = State(0);
        let waiters = WaitersRef::new();
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        let bound = ServiceBinding::new(probe.clone());
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
        assert_eq!(run(ctx.call(&bound, 1)), Err(Error));
        assert_eq!(probe.0.starts.get(), 2);
        assert_eq!(probe.0.calls.get(), 0);
    }

    #[test]
    fn cached_and_raw_calls_keep_their_target_and_state() {
        let waiters = WaitersRef::new();
        let st = State(10);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        let bound = ServiceBinding::new(probe.clone());
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
        assert_eq!(run(other_ctx.call(&bound, 5)), Ok(25));
        assert_eq!(probe.0.starts.get(), 3);
        assert_eq!(&*probe.0.states.borrow(), &[10, 10, 10]);

        probe.0.available.set(false);
        probe.0.changed.notify();
        assert_eq!(run(ctx.call_nowait(&bound, 6)), Ok(16));
        assert_eq!(probe.0.starts.get(), 3, "legacy bypass does not check");
        probe.0.available.set(true);
        probe.0.changed.notify();
        assert_eq!(run(ctx.call(&bound, 7)), Ok(17));
        assert_eq!(probe.0.starts.get(), 4);
    }

    struct Forward<S>(S);

    impl<S: Service<State, usize>> Service<State, usize> for Forward<S> {
        type Res = S::Res;
        type Error = S::Error;

        async fn ready(&self, ctx: Ctx<'_, Self, State>) -> Result<(), S::Error> {
            ctx.ready(&self.0).await
        }

        async fn call(&self, req: usize, ctx: Ctx<'_, Self, State>) -> Result<S::Res, S::Error> {
            ctx.call(&self.0, req).await
        }
    }

    #[test]
    fn fresh_clones_and_shared_aliases_retain_cache_hit_dependencies() {
        let waiters = WaitersRef::new();
        let st = State(0);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        let bound = ServiceBinding::new(probe.clone());
        assert_eq!(run(ctx.ready(&bound)), Ok(()));
        let fresh = bound.clone();
        assert!(!fresh.scope.is_ready());
        assert_eq!(run(ctx.ready(&fresh)), Ok(()));
        assert_eq!(probe.0.starts.get(), 2);

        let shared = Rc::new(bound);
        let alias = shared.clone();
        assert_eq!(run(ctx.ready(&alias)), Ok(()));
        let left = ServiceBinding::new(Forward(shared));
        let right = ServiceBinding::new(Forward(alias));
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

    #[test]
    fn raw_pending_checks_observe_notifications_without_caching() {
        let st = State(0);
        let waiters = WaitersRef::new();
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        probe.0.available.set(false);
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

        async fn ready(&self, ctx: Ctx<'_, Self, State>) -> Result<(), Error> {
            self.starts.set(self.starts.get() + 1);
            ctx.ready(&self.a).await?;
            ctx.ready(&self.b).await
        }

        async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Error> {
            Ok(req)
        }
    }

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

    #[test]
    fn stale_success_and_errors_are_not_cached() {
        let st = State(0);
        let waiters = WaitersRef::new();
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        probe.0.invalidate_ready.set(true);
        let bound = ServiceBinding::new(probe.clone());
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

    #[test]
    fn call_notifications_gate_the_next_call() {
        let st = State(0);
        let waiters = WaitersRef::new();
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        let bound = ServiceBinding::new(probe.clone());
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
        let bound =
            ServiceBinding::new(crate::boxed::service(crate::map_state(State(42), applied)));
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
}
