//! Reusable services and middleware.
//!
//! This module includes request buffering, in-flight request limits,
//! keep-alive handling, retries, per-request extensions, and timeouts.

pub mod buffer;
mod extensions;
pub mod inflight;
pub mod keepalive;
pub mod onerequest;
pub mod retry;
pub mod timeout;

#[doc(hidden)]
pub mod counter;

pub use self::counter::{Counter, CounterGuard};
pub use self::extensions::Extensions;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering};
    use std::task::{Context, Poll, Wake};
    use std::{cell::Cell, future::Future, future::poll_fn, rc::Rc};
    use std::{panic::AssertUnwindSafe, panic::catch_unwind};

    use ntex_service::{BindCtx, Ctx, Pipeline, ReadyCallback, ReadyNotifier, Service};

    use super::{inflight::InFlightService, onerequest::OneRequestService};
    use crate::{future::OnDropFn, task::LocalWaker};

    #[derive(Clone, Copy)]
    enum Outcome {
        Pending,
        Ok,
        Error,
        Panic,
    }

    struct Probe {
        changed: ReadyNotifier,
        checks: Cell<usize>,
        ready: Cell<Outcome>,
        call: Cell<Outcome>,
        active: Cell<usize>,
        waker: LocalWaker,
        panic_on_notify: Cell<bool>,
    }

    impl Service<(), ()> for Probe {
        type Res = ();
        type Error = ();

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.changed);
        }

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), ()> {
            self.checks.set(self.checks.get() + 1);
            poll_fn(|_| match self.ready.get() {
                Outcome::Pending => Poll::Pending,
                Outcome::Ok => Poll::Ready(Ok(())),
                Outcome::Error => Poll::Ready(Err(())),
                Outcome::Panic => panic!("inner readiness panic"),
            })
            .await
        }

        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), ()> {
            let active = self.active.replace(self.active.get() + 1);
            let _release = OnDropFn::new(|| self.active.set(self.active.get() - 1));
            assert_eq!(active, 0, "limiter allowed overlapping calls");
            poll_fn(|cx| match self.call.get() {
                Outcome::Pending => {
                    self.waker.register(cx.waker());
                    Poll::Pending
                }
                Outcome::Ok => Poll::Ready(Ok(())),
                Outcome::Error => Poll::Ready(Err(())),
                Outcome::Panic => panic!("inner call panic"),
            })
            .await
        }
    }

    struct Dispatch<S> {
        service: S,
        bypass: bool,
        callback: ReadyCallback,
    }

    impl<S: Service<(), (), Res = (), Error = ()>> Service<(), ()> for Dispatch<S> {
        type Res = ();
        type Error = ();

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.on_notify(&self.callback);
            ctx.bind(&self.service);
        }

        async fn ready(&self, ctx: Ctx<'_, Self>) -> Result<(), ()> {
            if self.bypass {
                Ok(())
            } else {
                ctx.ready(&self.service).await
            }
        }

        #[allow(deprecated, reason = "verify limiter safety under the legacy bypass")]
        async fn call(&self, req: (), ctx: Ctx<'_, Self>) -> Result<(), ()> {
            if self.bypass {
                ctx.call_nowait(&self.service, req).await
            } else {
                ctx.call(&self.service, req).await
            }
        }
    }

    fn shared<S>(service: S, bypass: bool, probe: Rc<Probe>) -> [Pipeline<(), (), ()>; 2]
    where
        S: Service<(), (), Res = (), Error = ()> + 'static,
    {
        let service = Rc::new(Dispatch {
            service,
            bypass,
            callback: ReadyCallback::new(move || {
                assert!(!probe.panic_on_notify.replace(false), "notification panic");
            }),
        });
        [
            Pipeline::new((), service.clone()),
            Pipeline::new((), service),
        ]
    }

    type SharedLimiter = (Rc<Probe>, [Pipeline<(), (), ()>; 2]);

    fn limiters(bypass: bool) -> [SharedLimiter; 2] {
        [false, true].map(|one_request| {
            let probe = Rc::new(Probe {
                changed: ReadyNotifier::new(),
                checks: Cell::new(0),
                ready: Cell::new(Outcome::Ok),
                call: Cell::new(Outcome::Ok),
                active: Cell::new(0),
                waker: LocalWaker::new(),
                panic_on_notify: Cell::new(false),
            });
            let services = if one_request {
                shared(OneRequestService::new(probe.clone()), bypass, probe.clone())
            } else {
                shared(
                    InFlightService::new(1, probe.clone()),
                    bypass,
                    probe.clone(),
                )
            };
            (probe, services)
        })
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[ntex::test]
    async fn inner_cache_survives_capacity_notifications() {
        for (probe, services) in limiters(false) {
            for service in &services {
                for _ in 0..3 {
                    service.ready().await.unwrap();
                    service.call(()).await.unwrap();
                }
            }
            assert_eq!(probe.checks.get(), 1);

            probe.ready.set(Outcome::Pending);
            probe.changed.notify();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            for service in &services {
                assert!(service.poll_ready(&mut cx).is_pending());
            }
            assert_eq!(probe.checks.get(), 2);

            probe.ready.set(Outcome::Ok);
            probe.changed.notify();
            for service in &services {
                service.ready().await.unwrap();
                service.call(()).await.unwrap();
            }
            assert_eq!(probe.checks.get(), 3);
        }
    }

    #[ntex::test]
    async fn shared_capacity_wakes_all_roots_and_releases() {
        for (probe, [first, second]) in limiters(false) {
            first.ready().await.unwrap();
            second.ready().await.unwrap();
            probe.call.set(Outcome::Pending);
            let wakes1 = Arc::new(WakeCount::default());
            let wakes2 = Arc::new(WakeCount::default());
            let waker1 = wakes1.clone().into();
            let waker2 = wakes2.clone().into();
            let mut cx1 = Context::from_waker(&waker1);
            let mut cx2 = Context::from_waker(&waker2);

            let mut active = Box::pin(first.call(()));
            assert!(active.as_mut().poll(&mut cx1).is_pending());
            let mut waiting = Box::pin(second.call(()));
            assert!(waiting.as_mut().poll(&mut cx2).is_pending());
            assert_eq!(probe.active.get(), 1);
            assert!(first.poll_ready(&mut cx1).is_pending());
            assert!(second.poll_ready(&mut cx2).is_pending());
            let before1 = wakes1.0.load(Ordering::Relaxed);
            let before2 = wakes2.0.load(Ordering::Relaxed);

            drop(active);
            assert_eq!(probe.active.get(), 0);
            assert!(wakes1.0.load(Ordering::Relaxed) > before1);
            assert!(wakes2.0.load(Ordering::Relaxed) > before2);
            assert!(waiting.as_mut().poll(&mut cx2).is_pending());
            assert_eq!(probe.active.get(), 1);
            assert!(first.poll_ready(&mut cx1).is_pending());

            probe.call.set(Outcome::Ok);
            probe.waker.wake();
            assert_eq!(waiting.as_mut().poll(&mut cx2), Poll::Ready(Ok(())));
            assert_eq!(probe.active.get(), 0);
            first.ready().await.unwrap();
            second.ready().await.unwrap();

            probe.call.set(Outcome::Error);
            assert_eq!(first.call(()).await, Err(()));
            assert_eq!(probe.active.get(), 0);
            second.ready().await.unwrap();
            assert_eq!(probe.checks.get(), 1);
        }
    }

    #[ntex::test]
    async fn bypass_still_checks_inner_and_releases_pending_permit() {
        for (probe, [first, second]) in limiters(true) {
            probe.ready.set(Outcome::Pending);
            let mut cx = Context::from_waker(std::task::Waker::noop());
            let mut active = Box::pin(first.call(()));
            assert!(active.as_mut().poll(&mut cx).is_pending());
            assert_eq!(probe.checks.get(), 1);
            assert_eq!(probe.active.get(), 0);
            let mut waiting = Box::pin(second.call(()));
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            drop(active);

            probe.ready.set(Outcome::Ok);
            probe.changed.notify();
            assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Ready(Ok(())));

            probe.ready.set(Outcome::Error);
            probe.changed.notify();
            assert_eq!(first.call(()).await, Err(()));
            assert_eq!(probe.active.get(), 0);
            probe.ready.set(Outcome::Ok);
            probe.changed.notify();
            second.call(()).await.unwrap();

            probe.ready.set(Outcome::Panic);
            probe.changed.notify();
            let mut call = Box::pin(first.call(()));
            assert!(catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(&mut cx))).is_err());
            drop(call);
            probe.ready.set(Outcome::Ok);
            probe.changed.notify();
            second.call(()).await.unwrap();
        }
    }

    #[ntex::test]
    async fn panicking_notification_or_inner_call_releases_permit() {
        for (probe, [first, second]) in limiters(false) {
            first.ready().await.unwrap();
            second.ready().await.unwrap();
            let mut cx = Context::from_waker(std::task::Waker::noop());

            probe.panic_on_notify.set(true);
            let mut call = Box::pin(first.call(()));
            assert!(catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(&mut cx))).is_err());
            drop(call);
            assert_eq!(probe.active.get(), 0);
            second.call(()).await.unwrap();

            probe.call.set(Outcome::Panic);
            let mut call = Box::pin(first.call(()));
            assert!(catch_unwind(AssertUnwindSafe(|| call.as_mut().poll(&mut cx))).is_err());
            drop(call);
            assert_eq!(probe.active.get(), 0);
            probe.call.set(Outcome::Ok);
            second.call(()).await.unwrap();
        }

        for (probe, [first, second]) in limiters(true) {
            probe.call.set(Outcome::Pending);
            let wakes = Arc::new(WakeCount::default());
            let waker = wakes.clone().into();
            let mut cx = Context::from_waker(&waker);
            let mut active = Box::pin(first.call(()));
            assert!(active.as_mut().poll(&mut cx).is_pending());
            let mut waiting = Box::pin(second.call(()));
            assert!(waiting.as_mut().poll(&mut cx).is_pending());
            let before = wakes.0.load(Ordering::Relaxed);

            probe.panic_on_notify.set(true);
            assert!(catch_unwind(AssertUnwindSafe(|| drop(active))).is_err());
            assert_eq!(probe.active.get(), 0);
            assert!(wakes.0.load(Ordering::Relaxed) > before);
            probe.call.set(Outcome::Ok);
            assert_eq!(waiting.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
        }
    }
}
