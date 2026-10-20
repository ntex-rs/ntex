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
        ctx.scope().depend_on(&self.scope);
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
    use crate::{Pipeline, ReadyNotifier, ServiceCaller, ctx::WaitersRef, pipeline::PipelineState};

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
        waker: RefCell<Option<Waker>>,
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

    #[test]
    fn retained_checker_wakes_every_live_observer_even_from_another_thread() {
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
        std::thread::spawn(move || wake.wake()).join().unwrap();
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
        let erased: crate::boxed::BoxService<(), usize, usize, &str> =
            crate::boxed::service(mapped);
        let alias = erased.clone();
        assert_eq!(run(ctx.call(&&erased, 1)), Ok(111));
        let left = ServiceBinding::new(alias);
        let right = ServiceBinding::new(crate::map_state(State(20), bound.clone()));
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
        assert_eq!(run(ctx.ready(&mapped)), Ok(()));
        assert_eq!(run(ctx.ready(&fresh)), Ok(()));
        assert_eq!(&*probe.0.states.borrow(), &[10, 20, 30, 30]);
    }

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
        let erased: crate::boxed::BoxService<(), usize, usize, &str> =
            crate::boxed::service(mapped);
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

        crate::forward_pl_ready!(State, inner);
        crate::forward_pl_shutdown!(State, inner);

        async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Error> {
            self.inner.call(req).await
        }
    }

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

        crate::forward_pl_ready!(State, inner, |_| "unavailable");

        async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Self::Error> {
            self.inner.call(req).await.map_err(|_| "unavailable")
        }
    }

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

        crate::forward_pl_shutdown!(State, inner);

        async fn call(&self, req: usize, _: Ctx<'_, Self, State>) -> Result<usize, Error> {
            self.inner.call(req).await
        }
    }

    #[test]
    fn shutdown_forwarding_registers_dependencies_without_a_readiness_check() {
        let waiters = WaitersRef::new();
        let st = State(0);
        let ctx = Ctx::<(), State>::new(0, &waiters, &st);
        let probe = Probe::new();
        let parent = ServiceBinding::new(ShutdownForward {
            inner: Pipeline::new(State(10), probe.clone()),
        });
        assert_eq!(run(ctx.ready(&parent)), Ok(()));
        assert_eq!(probe.0.starts.get(), 0);
        run(ctx.shutdown(&parent));
        assert!(!parent.scope.is_ready());
        assert_eq!(run(ctx.ready(&parent)), Ok(()));
        probe.0.shutdown_changed.notify();
        assert!(!parent.scope.is_ready());
        assert_eq!(probe.0.shutdown_state.get(), 10);
    }

    #[test]
    fn erased_binding_shutdown_subscriptions_reach_a_previously_ready_parent() {
        struct ShutdownOnly<S> {
            inner: S,
        }

        impl<S: Service<State, usize>> Service<State, usize> for ShutdownOnly<S> {
            type Res = S::Res;
            type Error = S::Error;

            crate::forward_shutdown!(State, inner);

            async fn call(
                &self,
                req: usize,
                ctx: Ctx<'_, Self, State>,
            ) -> Result<S::Res, S::Error> {
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
        assert_eq!(run(ctx.ready(&parent)), Ok(()));
        run(ctx.shutdown(&parent));
        assert_eq!(probe.0.starts.get(), 0);
        assert_eq!(probe.0.shutdown_state.get(), 42);
        probe.0.shutdown_changed.notify();
        assert!(!parent.scope.is_ready());
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
