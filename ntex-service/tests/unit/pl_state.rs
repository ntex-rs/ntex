use std::{cell::Cell, future::Future, future::pending, task::Waker};

use ntex::{channel::condition, util::lazy};

use super::*;
use crate::ReadyNotifier;

struct Srv(Rc<Cell<usize>>, condition::Waiter, ReadyNotifier);

impl Service<usize, usize> for Srv {
    type Res = usize;
    type Error = ();

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.2);
    }

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), ()> {
        self.0.set(self.0.get() + 1);
        self.1.ready().await;
        Ok(())
    }

    async fn call(&self, req: usize, ctx: Ctx<'_, Self, usize>) -> Result<usize, ()> {
        if req == 0 { Err(()) } else { Ok(req + *ctx.st()) }
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, usize>) {
        self.0.set(self.0.get() + 100 * *ctx);
    }
}

/// STATEMENT: State-taking pipelines and their bindings share readiness across different state values.
///
/// HOW:
/// - Mix direct, bound, fixed-state, cloned, and polled calls while counting readiness checks.
/// - Verify state-dependent responses and that shutdown forces a new readiness check.
#[ntex::test]
async fn pipeline_state() {
    let cnt = Rc::new(Cell::new(0));
    let cond = condition::Condition::new();
    let pl = PipelineState::new(Srv(cnt.clone(), cond.wait(), ReadyNotifier::new()));
    assert!(format!("{pl:?}").contains("PipelineState"));

    cond.notify_and_lock(());
    assert_eq!(pl.ready(&1).await, Ok(()));
    assert_eq!(cnt.get(), 1);
    // state values and calls do not invalidate shared readiness
    assert_eq!(pl.call(1, &2).await, Ok(3));
    assert_eq!(cnt.get(), 1);
    assert_eq!(pl.call(0, &2).await, Err(()));
    assert_eq!(pl.call(2, &3).await, Ok(5));
    assert_eq!(cnt.get(), 1);

    let b = pl.bind();
    assert!(format!("{b:?}").contains("PipelineStateBinding"));
    let b2 = b.clone();
    drop(b);
    assert_eq!(b2.call(1, &10).await, Ok(11));
    assert_eq!(cnt.get(), 1);
    assert_eq!(pl.ready(&1).await, Ok(()));
    assert_eq!(b2.call(1, &20).await, Ok(21));
    assert_eq!(cnt.get(), 1);

    let b = pl.bind_state(7);
    assert_eq!(b.ready().await, Ok(()));
    assert_eq!(b.call(1).await, Ok(8));
    assert_eq!(cnt.get(), 1);
    assert_eq!(b.call(2).await, Ok(9));
    assert_eq!(b.clone().call_static(3).await, Ok(10));
    assert_eq!(cnt.get(), 1);
    drop(b);

    assert_eq!(lazy(|cx| pl.poll_ready(cx, &1)).await, Poll::Ready(Ok(())));
    assert_eq!(cnt.get(), 1);
    assert_eq!(pl.call(1, &2).await, Ok(3));
    assert_eq!(cnt.get(), 1);

    // shutdown invalidates cached readiness
    assert_eq!(pl.ready(&1).await, Ok(()));
    pl.shutdown(&2).await;
    assert_eq!(cnt.get(), 201);
    assert_eq!(pl.call(1, &2).await, Ok(3));
    assert_eq!(cnt.get(), 202);
}

/// STATEMENT: `is_ready` reflects cached readiness and notifier invalidation for both pipeline types.
///
/// HOW:
/// - Check initially unready pipelines, complete readiness, and make a call.
/// - Notify each service and verify its pipeline reports unready again.
#[ntex::test]
async fn is_ready_tracks_shared_readiness() {
    let cond = condition::Condition::new();
    cond.notify_and_lock(());
    let changed = ReadyNotifier::new();
    let srv = Srv(Rc::default(), cond.wait(), changed.clone());
    let pl = PipelineState::new(srv);
    assert!(!pl.is_ready());
    assert_eq!(pl.ready(&1).await, Ok(()));
    assert!(pl.is_ready());
    assert_eq!(pl.call(1, &1).await, Ok(2));
    assert!(pl.is_ready());
    changed.notify();
    assert!(!pl.is_ready());

    let changed = ReadyNotifier::new();
    let pl = crate::Pipeline::new(1, Srv(Rc::default(), cond.wait(), changed.clone()));
    assert!(!pl.is_ready());
    assert_eq!(pl.ready().await, Ok(()));
    assert!(pl.is_ready());
    changed.notify();
    assert!(!pl.is_ready());
}

/// STATEMENT: State-bound readiness callers share one checker and recover when its owner is dropped.
///
/// HOW:
/// - Start a retained check, join it from a binding, and complete both with one service check.
/// - Invalidate, let a binding own the next check, then drop it and verify ownership transfers.
#[ntex::test]
async fn pipeline_state_poll_ready() {
    let cnt = Rc::new(Cell::new(0));
    let cond = condition::Condition::new();
    let changed = ReadyNotifier::new();
    let pl = PipelineState::new(Srv(cnt.clone(), cond.wait(), changed.clone()));

    assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
    assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
    assert_eq!(cnt.get(), 1);

    // binding waits while main readiness check is in progress
    let b = pl.bind_state(1);
    let mut fut = Box::pin(b.ready());
    assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
    assert_eq!(cnt.get(), 1);

    cond.notify(());
    assert_eq!(lazy(|cx| pl.poll_ready(cx, &1)).await, Poll::Ready(Ok(())));
    assert_eq!(cnt.get(), 1);

    // binding shares the completed readiness check
    assert_eq!(
        fut.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(()))
    );
    assert_eq!(cnt.get(), 1);
    drop(fut);

    changed.notify();
    let mut fut = Box::pin(b.ready());
    // binding owns a new readiness check after invalidation
    assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
    assert_eq!(cnt.get(), 2);
    assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
    assert_eq!(cnt.get(), 2);

    // dropping the owner releases the readiness check
    drop(fut);
    assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
    assert_eq!(cnt.get(), 3);
}

/// STATEMENT: Notifications, not calls or state changes, invalidate successful shared readiness.
///
/// HOW:
/// - Complete readiness, change the supplied state, and verify the cached result is reused.
/// - Notify and interleave a retained check with calls, checking one service check per invalidation.
#[ntex::test]
async fn pipeline_state_cached_readiness() {
    let cnt = Rc::new(Cell::new(0));
    let cond = condition::Condition::new();
    let changed = ReadyNotifier::new();
    let pl = PipelineState::new(Srv(cnt.clone(), cond.wait(), changed.clone()));

    let mut fut = Box::pin(pl.ready(&1));
    assert!(lazy(|cx| fut.as_mut().poll(cx)).await.is_pending());
    cond.notify(());
    assert_eq!(fut.await, Ok(()));
    assert_eq!(cnt.get(), 1);

    // only notification, not another caller, invalidates success
    assert_eq!(lazy(|cx| pl.poll_ready(cx, &2)).await, Poll::Ready(Ok(())));
    assert_eq!(cnt.get(), 1);
    changed.notify();
    assert!(lazy(|cx| pl.poll_ready(cx, &1)).await.is_pending());
    assert_eq!(cnt.get(), 2);
    let mut call = Box::pin(pl.call(1, &2));
    assert!(lazy(|cx| call.as_mut().poll(cx)).await.is_pending());
    assert_eq!(cnt.get(), 2);

    cond.notify(());
    assert_eq!(lazy(|cx| pl.poll_ready(cx, &1)).await, Poll::Ready(Ok(())));

    // the waiting call reuses the shared successful check
    assert_eq!(lazy(|cx| call.as_mut().poll(cx)).await, Poll::Ready(Ok(3)));
    assert_eq!(cnt.get(), 2);

    // calls do not consume readiness
    let mut call = Box::pin(pl.call(1, &2));
    assert_eq!(lazy(|cx| call.as_mut().poll(cx)).await, Poll::Ready(Ok(3)));
    assert_eq!(cnt.get(), 2);
    changed.notify();
    let mut call = Box::pin(pl.call(1, &2));
    assert!(lazy(|cx| call.as_mut().poll(cx)).await.is_pending());
    assert_eq!(cnt.get(), 3);
}

/// STATEMENT: Polling state-taking pipeline readiness after shutdown panics.
///
/// HOW:
/// - Complete shutdown on a `PipelineState`.
/// - Poll readiness and expect the explicit shutting-down panic.
#[ntex::test]
#[should_panic(expected = "Pipeline is shutting down")]
async fn pipeline_state_poll_ready_after_shutdown() {
    let cond = condition::Condition::new();
    let pl = PipelineState::new(Srv(Rc::default(), cond.wait(), ReadyNotifier::new()));
    pl.shutdown(&1).await;
    let _ = lazy(|cx| pl.poll_ready(cx, &1)).await;
}

struct Guard<'a>(&'a [usize]);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        assert_eq!(self.0.iter().sum::<usize>(), 3);
    }
}

struct Pending(Vec<usize>);

impl Service<usize, ()> for Pending {
    type Res = ();
    type Error = ();

    async fn ready(&self, _: Ctx<'_, Self, usize>) -> Result<(), ()> {
        let _g = Guard(&self.0);
        pending().await
    }

    async fn call(&self, (): (), _: Ctx<'_, Self, usize>) -> Result<(), ()> {
        Ok(())
    }
}

/// STATEMENT: Pending `PipelineState` readiness is dropped before its borrowed service data.
///
/// HOW:
/// - Poll readiness into a pending await with a guard borrowing the service's vector.
/// - Drop the pipeline and verify the guard can still read that vector.
#[test]
fn miri_drop_with_pending_readiness() {
    let mut cx = Context::from_waker(Waker::noop());

    let pl = PipelineState::new(Pending(vec![1, 2]));
    assert!(pl.poll_ready(&mut cx, &1).is_pending());
    drop(pl);
}
