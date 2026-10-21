use std::{cell::Cell, convert::Infallible, fmt, rc::Rc, rc::Weak, time};

use ntex_service::{BindCtx, Ctx, ReadyNotifier, Service, ServiceFactory};

use crate::time::{CallbackTimer, Millis, now};

/// Middleware that fails readiness once the service has been idle too long.
///
/// Every call records the current time and passes the request through
/// unchanged. When no call has been made for the keep-alive duration,
/// [`ready`](Service::ready) fails with the error produced by `f`.
/// A timer callback invalidates cached readiness on expiration, even when
/// readiness is not being polled. Extended deadlines are rearmed without
/// notifying readiness; active calls only update the activity timestamp.
pub struct KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    f: F,
    ka: Millis,
}

impl<F, E> KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    /// Creates keep-alive middleware.
    ///
    /// `ka` is the maximum idle time between calls, and `f` creates the error
    /// returned once it is exceeded.
    pub fn new(ka: Millis, f: F) -> Self {
        KeepAlive { f, ka }
    }
}

impl<F, E> Clone for KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    fn clone(&self) -> Self {
        KeepAlive {
            f: self.f.clone(),
            ka: self.ka,
        }
    }
}

impl<F, E> fmt::Debug for KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAlive")
            .field("ka", &self.ka)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, E, St, Req> ServiceFactory<St, Req> for KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    type Res = Req;
    type Error = E;

    type Service = KeepAliveService<F, E>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(KeepAliveService::new(self.ka, self.f.clone()))
    }
}

/// A service that fails readiness once it has been idle too long.
///
/// See [`KeepAlive`] for details.
pub struct KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    f: F,
    shared: Rc<Shared>,
}

struct Shared {
    dur: time::Duration,
    timer: CallbackTimer,
    expire: Cell<time::Instant>,
    expired: Cell<bool>,
    stopped: Cell<bool>,
    changed: ReadyNotifier,
}

impl Shared {
    fn on_timeout(&self) {
        let now = now();
        let expire = self.expire.get() + self.dur;
        if expire <= now {
            self.expired.set(true);
            self.changed.notify();
        } else {
            let millis: u32 = (expire - now).as_millis().try_into().unwrap_or(u32::MAX);
            self.timer.reset(Millis(millis.max(1)));
        }
    }
}

impl<F, E> KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    /// Creates a keep-alive service with the maximum idle time `dur`.
    ///
    /// `f` creates the error returned once the service has been idle longer
    /// than `dur`. The callback timer is canceled on shutdown or drop.
    pub fn new(dur: Millis, f: F) -> Self {
        KeepAliveService {
            f,
            shared: Rc::new_cyclic(|weak: &Weak<Shared>| {
                let weak = weak.clone();
                Shared {
                    dur: time::Duration::from(dur),
                    expire: Cell::new(now()),
                    expired: Cell::new(false),
                    stopped: Cell::new(false),
                    changed: ReadyNotifier::new(),
                    timer: CallbackTimer::new(dur, move || {
                        if let Some(shared) = weak.upgrade() {
                            shared.on_timeout();
                        }
                    }),
                }
            }),
        }
    }
}

impl<F, E> fmt::Debug for KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAliveService")
            .field("dur", &self.shared.dur)
            .field("expire", &self.shared.expire)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, E, St, Req> Service<St, Req> for KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    type Res = Req;
    type Error = E;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.shared.changed);
    }

    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        if self.shared.expire.get() + self.shared.dur <= now() {
            Err((self.f)())
        } else {
            Ok(())
        }
    }

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Req, E> {
        let shared = &self.shared;
        let now = now();
        let previous = shared.expire.replace(now);
        let expired = shared.expired.replace(false);
        if expired || previous + shared.dur <= now {
            if !shared.stopped.get() {
                shared.timer.reset(shared.dur.into());
            }
            shared.changed.notify();
        }
        Ok(req)
    }

    async fn shutdown(&self, _: Ctx<'_, Self, St>) {
        self.shared.stopped.set(true);
        self.shared.timer.cancel();
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::Pin, task::Context, task::Poll, task::ready};

    use ntex_service::{Pipeline, ReadyCallback, boxed, factory};

    use super::*;
    use crate::{channel::oneshot, spawn, time::sleep};

    #[derive(Debug, PartialEq)]
    struct TestErr;

    struct Dispatcher {
        p: Pipeline<usize, usize, TestErr>,
        tx: Option<oneshot::Sender<()>>,
    }

    impl Future for Dispatcher {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut this = self.as_mut();
            if ready!(this.p.poll_ready(cx)).is_err() {
                if let Some(tx) = this.tx.take() {
                    let _ = tx.send(());
                }
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    #[ntex::test]
    async fn test_ka() {
        // the keep-alive is much longer than the sleep between calls, so a
        // delayed wakeup does not expire it
        let factory = factory::<_, (), usize>(KeepAlive::new(Millis(500), || TestErr));
        assert!(format!("{factory:?}").contains("KeepAlive"));
        let _ = factory.clone();

        let svc = factory.create(&()).await.unwrap();
        assert!(format!("{svc:?}").contains("KeepAliveService"));

        let p = Pipeline::new((), boxed::service(svc));
        assert_eq!(p.call(1usize).await, Ok(1usize));
        let svc = p.bind();

        let (tx, rx) = oneshot::channel();
        spawn(Dispatcher { p, tx: Some(tx) }).detach();

        sleep(Millis(25)).await;
        assert_eq!(svc.call(1usize).await, Ok(1usize));
        sleep(Millis(100)).await;

        let res = rx.await;
        assert_eq!(res, Ok(()));
        assert_eq!(svc.ready().await, Err(TestErr));
    }

    #[ntex::test]
    async fn test_ka_sub_millis() {
        let svc = Rc::new(KeepAliveService::new(Millis(100), || TestErr));
        let p = Pipeline::<usize, usize, TestErr>::new((), svc.clone()).bind();
        svc.shared.timer.cancel();

        // less than millisecond is left before expiration
        svc.shared
            .expire
            .set(now().checked_sub(svc.shared.dur).unwrap() + time::Duration::from_micros(500));
        svc.shared.on_timeout();
        assert_eq!(p.ready().await, Ok(()));
        assert!(!svc.shared.expired.get());
        assert!(!svc.shared.timer.is_elapsed());
    }

    fn notification(p: &Pipeline<usize, usize, TestErr>) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let tx = Cell::new(Some(tx));
        p.on_notify(&ReadyCallback::new(move || {
            if let Some(tx) = tx.take() {
                let _ = tx.send(());
            }
        }));
        rx
    }

    #[ntex::test]
    async fn idle_timer_invalidates_completed_readiness_for_all_roots() {
        let svc = Rc::new(KeepAliveService::new(Millis(25), || TestErr));
        let first = Pipeline::new((), svc.clone());
        let second = Pipeline::<usize, usize, TestErr>::new((), svc);
        assert_eq!(first.ready().await, Ok(()));
        assert_eq!(second.ready().await, Ok(()));
        let expired = notification(&first);

        crate::time::timeout(Millis(1_000), expired)
            .await
            .unwrap()
            .unwrap();
        assert!(!first.is_ready());
        assert!(!second.is_ready());
        assert_eq!(first.ready().await, Err(TestErr));
        assert_eq!(second.ready().await, Err(TestErr));
        assert_eq!(first.call(1).await, Err(TestErr));
    }

    #[ntex::test]
    async fn call_extends_deadline_before_old_timer_is_processed() {
        let svc = Rc::new(KeepAliveService::new(Millis(250), || TestErr));
        let p = Pipeline::new((), svc.clone());
        let notifications = Rc::new(Cell::new(0));
        let count = notifications.clone();
        p.on_notify(&ReadyCallback::new(move || count.set(count.get() + 1)));
        assert_eq!(p.ready().await, Ok(()));
        svc.shared.timer.reset(Millis::ZERO);
        assert_eq!(p.call(1).await, Ok(1));
        sleep(Millis(50)).await;
        assert!(p.is_ready());
        assert_eq!(notifications.get(), 0);
        assert!(!svc.shared.expired.get());
        assert!(!svc.shared.timer.is_elapsed());

        crate::time::timeout(Millis(1_000), notification(&p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(notifications.get(), 1);
        assert_eq!(p.ready().await, Err(TestErr));
        assert_eq!(p.call(2).await, Err(TestErr));
    }

    struct Bypass<S>(S);

    impl<S: Service<(), usize, Res = usize, Error = TestErr>> Service<(), usize> for Bypass<S> {
        type Res = usize;
        type Error = TestErr;

        ntex_service::forward_bind!((), 0);

        #[allow(
            deprecated,
            reason = "verify reset after an intentional readiness bypass"
        )]
        async fn call(&self, req: usize, ctx: Ctx<'_, Self>) -> Result<usize, TestErr> {
            ctx.call_nowait(&self.0, req).await
        }
    }

    #[ntex::test]
    async fn bypassed_call_rearms_an_expired_timer() {
        let svc = Rc::new(KeepAliveService::new(Millis(25), || TestErr));
        let p = Pipeline::new((), svc.clone());
        let bypass = Pipeline::new((), Bypass(svc.clone()));
        p.ready().await.unwrap();
        crate::time::timeout(Millis(1_000), notification(&p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.ready().await, Err(TestErr));

        assert_eq!(bypass.call(1).await, Ok(1));
        assert!(!svc.shared.timer.is_elapsed());
        assert_eq!(p.ready().await, Ok(()));
        crate::time::timeout(Millis(1_000), notification(&p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.ready().await, Err(TestErr));
    }

    #[ntex::test]
    async fn timer_callback_is_canceled_on_shutdown_and_drop() {
        for shutdown in [false, true] {
            let svc = Rc::new(KeepAliveService::new(Millis(25), || TestErr));
            let state = Rc::downgrade(&svc.shared);
            let p = Pipeline::<usize, usize, TestErr>::new((), svc.clone());
            let notifications = Rc::new(Cell::new(0));
            let count = notifications.clone();
            p.on_notify(&ReadyCallback::new(move || count.set(count.get() + 1)));
            p.ready().await.unwrap();
            if shutdown {
                p.shutdown().await;
                assert!(svc.shared.timer.is_elapsed());
                sleep(Millis(75)).await;
                assert_eq!(notifications.get(), 0);

                let bypass = Pipeline::new((), Bypass(svc.clone()));
                assert_eq!(bypass.call(1).await, Ok(1));
                assert!(svc.shared.timer.is_elapsed());
                let before = notifications.get();
                sleep(Millis(75)).await;
                assert_eq!(notifications.get(), before);
            }
            let before = notifications.get();
            drop(p);
            drop(svc);
            assert!(state.upgrade().is_none());
            sleep(Millis(75)).await;
            assert_eq!(notifications.get(), before);
        }
    }

    #[ntex::test]
    async fn test_ka_zero() {
        let p = Pipeline::<usize, usize, TestErr>::new(
            (),
            KeepAliveService::new(Millis::ZERO, || TestErr),
        );
        assert_eq!(p.ready().await, Err(TestErr));
        assert_eq!(p.call(1).await, Err(TestErr));
    }
}
