use std::{cell::Cell, convert::Infallible, fmt, future::poll_fn, rc::Rc, task::Context};
use std::{task::Poll, time};

use ntex_service::{BindCtx, Ctx, ReadyNotifier, Service, ServiceFactory};

use crate::future::OnDropFn;
use crate::task::LocalWaker;
use crate::time::{Millis, Sleep, now, sleep};

/// Middleware that fails readiness once the service has been idle too long.
///
/// Every call records the current time and passes the request through
/// unchanged. When no call has been made for the keep-alive duration,
/// [`ready`](Service::ready) fails with the error produced by `f`.
/// A local timer task invalidates cached readiness on expiration, even when
/// readiness is not being polled.
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
    task: Cell<Option<ntex_rt::JoinHandle<()>>>,
}

struct Shared {
    dur: time::Duration,
    sleep: Sleep,
    expire: Cell<time::Instant>,
    expired: Cell<bool>,
    changed: ReadyNotifier,
    waker: LocalWaker,
}

impl Shared {
    fn poll_timer(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.waker.register(cx.waker());
        if self.expired.get() {
            return Poll::Pending;
        }

        loop {
            if self.sleep.poll_elapsed(cx).is_pending() {
                return Poll::Pending;
            }

            let now = now();
            let expire = self.expire.get() + self.dur;
            if expire <= now {
                self.expired.set(true);
                self.changed.notify();
                return Poll::Pending;
            }

            // Round a remaining sub-millisecond duration up so the timer re-arms.
            let millis: u32 = (expire - now).as_millis().try_into().unwrap_or(u32::MAX);
            self.sleep.reset(Millis(millis.max(1)));
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
    /// than `dur`. A service-owned timer task is canceled on shutdown or drop.
    pub fn new(dur: Millis, f: F) -> Self {
        let shared = Rc::new(Shared {
            expire: Cell::new(now()),
            sleep: sleep(dur),
            dur: time::Duration::from(dur),
            expired: Cell::new(false),
            changed: ReadyNotifier::new(),
            waker: LocalWaker::new(),
        });
        let timer = shared.clone();
        let task = crate::spawn(poll_fn(move |cx| timer.poll_timer(cx)));

        KeepAliveService {
            f,
            shared,
            task: Cell::new(Some(task)),
        }
    }

    fn stop(&self) {
        if let Some(task) = self.task.take() {
            task.cancel();
        }
    }
}

impl<F, E> Drop for KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    fn drop(&mut self) {
        self.stop();
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
            shared.sleep.reset(shared.dur);
            let _wake = OnDropFn::new(|| shared.waker.wake());
            shared.changed.notify();
        }
        Ok(req)
    }

    async fn shutdown(&self, _: Ctx<'_, Self, St>) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::{pin::Pin, task::Context, task::Poll, task::ready};

    use ntex_service::{Pipeline, ReadyCallback, boxed, factory};

    use super::*;
    use crate::{channel::oneshot, spawn};

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

        // less than millisecond is left before expiration
        svc.shared
            .expire
            .set(now().checked_sub(svc.shared.dur).unwrap() + time::Duration::from_micros(500));
        svc.shared.sleep.elapse();

        let p = Pipeline::<usize, usize, TestErr>::new((), svc.clone()).bind();
        assert_eq!(p.ready().await, Ok(()));

        // timer has to be re-armed, otherwise waker never gets registered
        // and service readiness never resolves
        assert!(
            crate::future::lazy(|cx| svc.shared.poll_timer(cx))
                .await
                .is_pending()
        );
        assert!(!svc.shared.sleep.is_elapsed());
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
        assert_eq!(first.ready().await, Err(TestErr));
        assert_eq!(second.ready().await, Err(TestErr));
        assert_eq!(first.call(1).await, Err(TestErr));
    }

    #[ntex::test]
    async fn call_extends_deadline_before_old_timer_is_processed() {
        let svc = Rc::new(KeepAliveService::new(Millis(250), || TestErr));
        let p = Pipeline::new((), svc.clone());
        assert_eq!(p.ready().await, Ok(()));
        svc.shared.sleep.elapse();
        assert_eq!(p.call(1).await, Ok(1));
        sleep(Millis(25)).await;
        assert_eq!(p.ready().await, Ok(()));
        assert!(!svc.shared.expired.get());
        assert!(!svc.shared.sleep.is_elapsed());

        crate::time::timeout(Millis(1_000), notification(&p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.ready().await, Err(TestErr));
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
        let bypass = Pipeline::new((), Bypass(svc));
        p.ready().await.unwrap();
        crate::time::timeout(Millis(1_000), notification(&p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.ready().await, Err(TestErr));

        assert_eq!(bypass.call(1).await, Ok(1));
        assert_eq!(p.ready().await, Ok(()));
        crate::time::timeout(Millis(1_000), notification(&p))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.ready().await, Err(TestErr));
    }

    #[ntex::test]
    async fn timer_task_is_canceled_on_shutdown_and_drop() {
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
                sleep(Millis(75)).await;
                assert_eq!(notifications.get(), 0);
            }
            drop(p);
            drop(svc);
            sleep(Millis(75)).await;
            assert_eq!(notifications.get(), 0);
            assert!(state.upgrade().is_none());
        }
    }
}
