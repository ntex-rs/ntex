use std::{future::pending, task::Waker};

use super::*;
use crate::Pipeline;

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
        let _g = Guard(&self.0);
        pending().await
    }

    async fn shutdown(&self, _: Ctx<'_, Self, usize>) {
        let _g = Guard(&self.0);
        pending::<()>().await;
    }
}

/// STATEMENT: Dropping a pipeline with pending readiness preserves borrowed service data during cleanup.
///
/// HOW:
/// - Poll readiness until a guard borrows the service's vector across a pending await.
/// - Drop the pipeline and let the guard verify the vector remains valid.
#[test]
fn miri_drop_with_pending_readiness() {
    let mut cx = Context::from_waker(Waker::noop());
    let pl = Pipeline::new(1, Pending(vec![1, 2]));
    assert!(pl.poll_ready(&mut cx).is_pending());
    drop(pl);
}

/// STATEMENT: Dropping a pipeline with pending shutdown safely destroys the borrowing shutdown future.
///
/// HOW:
/// - Poll shutdown to pending and verify the pipeline is marked shutting down.
/// - Drop it while a guard checks borrowed service data during future destruction.
#[test]
fn miri_drop_with_pending_shutdown() {
    let mut cx = Context::from_waker(Waker::noop());
    let pl = Pipeline::new(1, Pending(vec![1, 2]));
    assert!(pl.poll_shutdown(&mut cx).is_pending());
    assert!(pl.is_shutdown());
    drop(pl);
}

/// STATEMENT: An owned call retains service data after the original pipeline is dropped.
///
/// HOW:
/// - Create an owned call, drop the pipeline, and poll the call into pending readiness.
/// - Drop the call and verify its readiness guard can still read the service's vector.
#[test]
fn miri_drop_pending_call_after_pipeline() {
    let mut cx = Context::from_waker(Waker::noop());
    let pl = Pipeline::new(1, Pending(vec![1, 2]));
    let mut call = Box::pin(pl.call_static(()));
    drop(pl);
    assert!(call.as_mut().poll(&mut cx).is_pending());
    drop(call);
}
