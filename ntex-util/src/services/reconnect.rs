use std::{
    cell::Cell, cell::RefCell, error::Error, fmt, marker::PhantomData, rc::Rc, time::Instant,
};

use backoff::{ExponentialBackoff, backoff::Backoff};
use ntex_error::fmt_err_string;
use ntex_service::{BindCtx, Ctx, ReadyNotifier, Service, ServiceFactory};

use crate::time;

/// Reconnect is a service that generates `ReconnectService` for
/// every `request`.
#[derive(Debug)]
pub struct Reconnect<P, Sf, St> {
    name: &'static str,
    policy: P,
    factory: Rc<Sf>,
    retry: usize,
    max_retry: usize,
    ph: PhantomData<St>,
}

pub trait ReconnectPolicy<E>: Clone + Sized + 'static {
    fn reconnect(&self, err: &E) -> bool;
}

#[derive(Copy, Clone, Debug)]
pub struct DefaultPolicy;

impl<E> ReconnectPolicy<E> for DefaultPolicy {
    fn reconnect(&self, _: &E) -> bool {
        true
    }
}

impl<T, E> ReconnectPolicy<E> for T
where
    T: Fn(&E) -> bool + Clone + 'static,
{
    fn reconnect(&self, err: &E) -> bool {
        (*self)(err)
    }
}

impl<Sf, St> Reconnect<DefaultPolicy, Sf, St>
where
    St: 'static,
    Sf: 'static,
{
    pub fn new<Req>(name: &'static str, factory: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req> + 'static,
        Sf::InitError: Into<Sf::Error>,
    {
        Self {
            name,
            factory: Rc::new(factory),
            policy: DefaultPolicy,
            retry: 3,
            max_retry: 3,
            ph: PhantomData,
        }
    }
}

impl<P, Sf, St> Reconnect<P, Sf, St> {
    /// Sets the retry budget, restored after each successful readiness check.
    #[must_use]
    pub fn retry(mut self, retry: usize) -> Self {
        self.retry = retry;
        self.max_retry = retry;
        self
    }

    pub fn policy<T, Req>(self, policy: T) -> Reconnect<T, Sf, St>
    where
        T: ReconnectPolicy<Sf::Error>,
        Sf: ServiceFactory<St, Req>,
    {
        Reconnect {
            policy,
            name: self.name,
            factory: self.factory,
            retry: self.retry,
            max_retry: self.max_retry,
            ph: PhantomData,
        }
    }
}

impl<P, Sf, St> Clone for Reconnect<P, Sf, St>
where
    P: Clone,
{
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            policy: self.policy.clone(),
            factory: self.factory.clone(),
            retry: self.retry,
            max_retry: self.max_retry,
            ph: PhantomData,
        }
    }
}

impl<P, Sf, St, Req> ServiceFactory<St, Req> for Reconnect<P, Sf, St>
where
    P: ReconnectPolicy<Sf::Error>,
    Sf: ServiceFactory<St, Req>,
    Sf::Error: Error,
    Sf::InitError: Into<Sf::Error> + Error,
    St: 'static,
{
    type Res = Sf::Res;
    type Error = Sf::Error;

    type Service = ReconnectService<P, Sf, Sf::Service, St>;
    type InitError = Sf::Error;

    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(ReconnectService {
            name: self.name,
            policy: self.policy.clone(),
            factory: self.factory.clone(),
            service: Cell::new(None),
            err: Cell::new(false),
            changed: ReadyNotifier::new(),
            retry: Cell::new(self.retry),
            max_retry: self.max_retry,
            backoff: RefCell::new(ExponentialBackoff::default()),
            retry_at: Cell::new(None),
            ph: PhantomData,
        })
    }
}

/// `ReconnectService` uses factory service and stored request for
/// service creation. If service's `.poll_ready()` returns error
/// service get dropped and new service get created from original
/// request and supplied factory. If factory returns error
/// during service creation, `ReconnectService` retries service creation
/// process, number of retries is supplied by Reconnect service.
/// Exponential backoff is used for sleep period between retries.
///
/// Clones start disconnected and create their own inner service on the next
/// readiness check.
pub struct ReconnectService<P, Sf, S, St> {
    name: &'static str,
    policy: P,
    factory: Rc<Sf>,
    service: Cell<Option<Rc<S>>>,
    err: Cell<bool>,
    changed: ReadyNotifier,
    retry: Cell<usize>,
    max_retry: usize,
    backoff: RefCell<ExponentialBackoff>,
    // Keep the retry deadline when a readiness check is cancelled.
    retry_at: Cell<Option<Instant>>,
    ph: PhantomData<St>,
}

impl<P, Sf, S, St> fmt::Debug for ReconnectService<P, Sf, S, St> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconnectService")
            .field("name", &self.name)
            .field("policy", &std::any::type_name::<P>())
            .field("factory", &std::any::type_name::<Sf>())
            .field("service", &std::any::type_name::<S>())
            .field("err", &self.err)
            .field("retry", &self.retry)
            .field("max_retry", &self.max_retry)
            .field("retry_at", &self.retry_at)
            .finish()
    }
}

impl<P: Clone, Sf, S, St> Clone for ReconnectService<P, Sf, S, St> {
    fn clone(&self) -> Self {
        Self {
            name: self.name,
            policy: self.policy.clone(),
            factory: self.factory.clone(),
            service: Cell::new(None),
            err: Cell::new(false),
            changed: ReadyNotifier::new(),
            retry: Cell::new(self.retry.get()),
            max_retry: self.max_retry,
            backoff: RefCell::new(self.backoff.borrow().clone()),
            retry_at: Cell::new(None),
            ph: PhantomData,
        }
    }
}

impl<Sf, S, St> ReconnectService<DefaultPolicy, Sf, S, St>
where
    St: 'static,
    Sf: 'static,
{
    pub fn new<Req>(name: &'static str, factory: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Service = S, Error = S::Error> + 'static,
        Sf::Error: Error,
        Sf::InitError: Into<Sf::Error> + Error,
        S: Service<St, Req>,
    {
        Self {
            name,
            policy: DefaultPolicy,
            factory: Rc::new(factory),
            service: Cell::new(None),
            err: Cell::new(false),
            changed: ReadyNotifier::new(),
            retry: Cell::new(3),
            max_retry: 3,
            backoff: RefCell::new(ExponentialBackoff::default()),
            retry_at: Cell::new(None),
            ph: PhantomData,
        }
    }
}

impl<P, Sf, S, St> ReconnectService<P, Sf, S, St> {
    /// Sets the retry budget, restored after each successful readiness check.
    #[must_use]
    pub fn retry(mut self, retry: usize) -> Self {
        self.retry.set(retry);
        self.max_retry = retry;
        self
    }

    pub fn policy<T, Req>(self, policy: T) -> ReconnectService<T, Sf, S, St>
    where
        T: ReconnectPolicy<Sf::Error>,
        Sf: ServiceFactory<St, Req>,
    {
        ReconnectService {
            policy,
            name: self.name,
            factory: self.factory,
            service: self.service,
            err: self.err,
            changed: self.changed,
            retry: self.retry,
            max_retry: self.max_retry,
            backoff: self.backoff,
            retry_at: self.retry_at,
            ph: PhantomData,
        }
    }
}

impl<P, Sf, S, St> ReconnectService<P, Sf, S, St> {
    fn current(&self) -> Option<Rc<S>> {
        let svc = self.service.take();
        self.service.set(svc.clone());
        svc
    }

    fn remove(&self, svc: &Rc<S>) {
        if let Some(current) = self.service.take()
            && !Rc::ptr_eq(&current, svc)
        {
            self.service.set(Some(current));
        }
    }

    /// Returns a ready inner service, (re)creating it if needed.
    ///
    /// The service stays stored while its readiness is checked, so concurrent
    /// calls can use it; ready pipelines can dispatch without calling `ready()`.
    async fn ensure<Req>(&self, ctx: Ctx<'_, Self, St>) -> Result<Rc<S>, S::Error>
    where
        P: ReconnectPolicy<Sf::Error>,
        Sf: ServiceFactory<St, Req, Service = S, Error = S::Error>,
        Sf::Error: Error,
        Sf::InitError: Into<Sf::Error> + Error,
        S: Service<St, Req>,
        St: 'static,
    {
        if self.err.get() {
            log::trace!("State marked as errored, reconnecting: {:?}", self.name);
            self.err.set(false);
            let _ = self.service.take();
        }

        loop {
            let svc = if let Some(svc) = self.current() {
                svc
            } else {
                if let Some(retry_at) = self.retry_at.get() {
                    let now = time::now();
                    if now < retry_at {
                        time::sleep(retry_at - now).await;
                        continue;
                    }
                    self.retry_at.set(None);
                }

                // construct new service
                match ctx.create(self.factory.as_ref()).await {
                    Ok(service) => {
                        // reset backoff and retry
                        log::trace!("Re-connect is done, service '{}' is creates", self.name);
                        self.backoff.borrow_mut().reset();
                        self.retry_at.set(None);
                        let svc = Rc::new(service);
                        self.service.set(Some(svc.clone()));
                        svc
                    }
                    Err(err) => {
                        let s = fmt_err_string(&err);
                        log::trace!("Failed to create service: {s}");

                        // check policy
                        let err = err.into();
                        if self.policy.reconnect(&err) {
                            // start backoff sleep
                            let retry = self.retry.get();
                            if retry > 0 {
                                let timeout = self.backoff.borrow_mut().next_backoff();
                                if let Some(timeout) = timeout {
                                    self.retry.set(retry - 1);
                                    log::trace!(
                                        "Error during re-connecting '{}': {s}, sleep: {timeout:?}",
                                        self.name
                                    );
                                    self.retry_at.set(Some(Instant::now() + timeout));
                                    log::trace!(
                                        "Re-connecting, checking service '{}' readiness",
                                        self.name
                                    );
                                    continue;
                                }
                            }
                        }
                        self.err.set(true);
                        return Err(err);
                    }
                }
            };

            // connected
            break if let Err(err) = ctx.ready(svc.as_ref()).await {
                log::trace!(
                    "Error during service '{}' readiness check: {}",
                    self.name,
                    fmt_err_string(&err)
                );
                self.remove(&svc);

                // check policy
                if self.policy.reconnect(&err) {
                    // start backoff sleep
                    let retry = self.retry.get();
                    if retry > 0 {
                        let timeout = self.backoff.borrow_mut().next_backoff();
                        if let Some(timeout) = timeout {
                            self.retry.set(retry - 1);
                            log::trace!(
                                "Error during checking '{}' readiness: {}, sleep: {timeout:?}",
                                self.name,
                                fmt_err_string(&err)
                            );
                            self.retry_at.set(Some(Instant::now() + timeout));
                            continue;
                        }
                    }
                }

                self.err.set(true);
                Err(err)
            } else {
                self.retry.set(self.max_retry);
                Ok(svc)
            };
        }
    }
}

impl<P, Sf, S, St, Req> Service<St, Req> for ReconnectService<P, Sf, S, St>
where
    P: ReconnectPolicy<Sf::Error>,
    Sf: ServiceFactory<St, Req, Service = S, Error = S::Error>,
    Sf::Error: Error,
    Sf::InitError: Into<Sf::Error> + Error,
    S: Service<St, Req>,
    St: 'static,
{
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.changed);
        if let Some(svc) = self.current() {
            ctx.bind(svc.as_ref());
        }
    }

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.ensure(ctx).await.map(|_| ())
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        if let Some(svc) = self.current() {
            ctx.shutdown(svc.as_ref()).await;
        }
    }

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        // pipeline readiness is cached, the service could fail after the last readiness check
        let svc = match self.current() {
            Some(svc) if !self.err.get() => svc,
            _ => self.ensure(ctx).await?,
        };
        let result = ctx.call(svc.as_ref(), req).await;

        if let Err(ref err) = result {
            log::trace!("Service failed: {}", fmt_err_string(err));
            // make sure current service is the same
            if self
                .current()
                .is_some_and(|current| Rc::ptr_eq(&current, &svc))
                && self.policy.reconnect(err)
            {
                self.err.set(true);
                self.changed.notify();
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use std::{future::Future, future::poll_fn, io, pin::pin, task::Context, task::Poll};

    use ntex_service::{Pipeline, ReadyBinding, fn_factory, fn_service};

    use super::*;
    use crate::channel::oneshot;

    struct Probe {
        id: usize,
        ready: Cell<bool>,
        changed: ReadyNotifier,
    }

    impl Service<(), ()> for Probe {
        type Res = usize;
        type Error = io::Error;

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.changed);
        }

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
            poll_fn(|_| {
                if self.ready.get() {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
            .await
        }

        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<usize, io::Error> {
            Ok(self.id)
        }
    }

    /// STATEMENT: Cloned reconnect services lazily create independent children and readiness gates.
    ///
    /// HOW:
    /// - Clone before and after the original connects, counting factory calls and child identities.
    /// - Wrap each child in `ReadyBinding` and place the reconnect services in separate pipelines.
    /// - Verify cloning is lazy and both pipelines retain their own child across calls.
    /// - Make each child unavailable and verify only its pipeline waits until readiness is restored.
    #[ntex::test]
    async fn clones_create_independent_children() {
        for connected in [false, true] {
            let children = Rc::new(RefCell::new(Vec::new()));
            let factory = fn_factory({
                let children = children.clone();
                move |(): &()| {
                    let mut children = children.borrow_mut();
                    let child = Rc::new(Probe {
                        id: children.len(),
                        ready: Cell::new(true),
                        changed: ReadyNotifier::new(),
                    });
                    children.push(child.clone());
                    async move { Ok::<_, io::Error>(ReadyBinding::new(child)) }
                }
            });
            let service = Rc::new(ReconnectService::new("clone", factory));
            let original = Pipeline::new((), service.clone());
            if connected {
                original.ready().await.unwrap();
            }

            let cloned = Pipeline::new((), service.as_ref().clone());
            assert_eq!(children.borrow().len(), usize::from(connected));
            original.ready().await.unwrap();
            cloned.ready().await.unwrap();
            assert_eq!(children.borrow().len(), 2);
            assert_eq!(original.call(()).await.unwrap(), 0);
            assert_eq!(cloned.call(()).await.unwrap(), 1);

            let mut cx = Context::from_waker(std::task::Waker::noop());
            for (id, pipeline, other) in [(0, &original, &cloned), (1, &cloned, &original)] {
                let child = children.borrow()[id].clone();
                child.ready.set(false);
                child.changed.notify();
                assert!(!pipeline.is_ready());
                assert!(other.is_ready());

                let mut call = pin!(pipeline.call(()));
                assert!(call.as_mut().poll(&mut cx).is_pending());
                assert_eq!(other.call(()).await.unwrap(), 1 - id);

                child.ready.set(true);
                child.changed.notify();
                assert_eq!(call.await.unwrap(), id);
            }
            assert_eq!(children.borrow().len(), 2);
        }
    }

    /// STATEMENT: A late error from a retired child does not invalidate its healthy replacement.
    ///
    /// HOW:
    /// - Hold a call on the first child, then fail another call to trigger reconnection.
    /// - Establish the replacement and release the held call with an error.
    /// - Verify the error is returned without invalidating readiness or creating another child.
    #[ntex::test]
    async fn late_call_error_preserves_replacement() {
        let created = Rc::new(Cell::new(0));
        let factory = fn_factory({
            let created = created.clone();
            move |(): &()| {
                let id = created.get();
                created.set(id + 1);
                async move {
                    Ok::<_, io::Error>(fn_service(
                        move |fail: Option<oneshot::Receiver<()>>| async move {
                            if let Some(fail) = fail {
                                fail.await.unwrap();
                                Err(io::Error::other("call failed"))
                            } else {
                                Ok(id)
                            }
                        },
                    ))
                }
            }
        });
        let pipeline = Pipeline::new((), ReconnectService::new("late-error", factory));
        let (late_tx, late_rx) = oneshot::channel();
        let mut late_call = pin!(pipeline.call(Some(late_rx)));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(late_call.as_mut().poll(&mut cx).is_pending());
        assert_eq!(created.get(), 1);

        let (fail_tx, fail_rx) = oneshot::channel();
        fail_tx.send(()).unwrap();
        assert!(pipeline.call(Some(fail_rx)).await.is_err());
        assert!(!pipeline.is_ready());
        assert_eq!(pipeline.call(None).await.unwrap(), 1);
        assert!(pipeline.is_ready());

        late_tx.send(()).unwrap();
        assert!(late_call.await.is_err());
        assert!(pipeline.is_ready());
        assert_eq!(pipeline.call(None).await.unwrap(), 1);
        assert_eq!(created.get(), 2);
    }

    /// STATEMENT: Retry delays survive readiness invalidation and cancellation.
    ///
    /// HOW:
    /// - Fail creation or initial child readiness with a deterministic thirty-second backoff.
    /// - Repeatedly poll, invalidate readiness, and replace the pending readiness future.
    /// - Make the factory healthy and verify no new creation occurs before the retry delay expires.
    /// - Confirm a clone connects independently without inheriting the pending delay.
    /// - Shorten the original deadline and verify it reconnects and resets backoff after expiry.
    #[ntex::test]
    async fn retry_delay_survives_readiness_restarts() {
        struct FailReady(Rc<Cell<bool>>);

        impl Service<(), ()> for FailReady {
            type Res = ();
            type Error = io::Error;

            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                if self.0.get() {
                    Err(io::Error::other("readiness failed"))
                } else {
                    Ok(())
                }
            }

            async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), io::Error> {
                Ok(())
            }
        }

        for fail_creation in [false, true] {
            let created = Rc::new(Cell::new(0));
            let failing = Rc::new(Cell::new(true));
            let factory = fn_factory({
                let created = created.clone();
                let failing = failing.clone();
                move |(): &()| {
                    created.set(created.get() + 1);
                    let failing = failing.clone();
                    async move {
                        if fail_creation && failing.get() {
                            Err(io::Error::other("creation failed"))
                        } else {
                            Ok(FailReady(failing))
                        }
                    }
                }
            });
            let service = Rc::new(ReconnectService::new("retry-delay", factory));
            {
                let mut backoff = service.backoff.borrow_mut();
                backoff.initial_interval = Duration::from_secs(30);
                backoff.randomization_factor = 0.0;
                backoff.reset();
            }
            let pipeline = Pipeline::new((), service.clone());
            let mut cx = Context::from_waker(std::task::Waker::noop());
            {
                let mut ready = pin!(pipeline.ready());
                for _ in 0..3 {
                    assert!(ready.as_mut().poll(&mut cx).is_pending());
                    assert_eq!(created.get(), 1);
                }
                service.changed.notify();
                assert!(ready.as_mut().poll(&mut cx).is_pending());
                assert_eq!(created.get(), 1);
            }
            let retry_at = service.retry_at.get();
            failing.set(false);
            {
                let mut ready = pin!(pipeline.ready());
                assert!(ready.as_mut().poll(&mut cx).is_pending());
                assert_eq!(created.get(), 1);
            }
            assert_eq!(service.retry_at.get(), retry_at);

            let cloned = Pipeline::new((), service.as_ref().clone());
            {
                let mut ready = pin!(cloned.ready());
                assert!(matches!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
            }
            assert_eq!(created.get(), 2);

            service
                .retry_at
                .set(Some(Instant::now() + Duration::from_millis(1)));
            pipeline.ready().await.unwrap();
            pipeline.call(()).await.unwrap();
            assert_eq!(created.get(), 3);
            assert_eq!(
                service.backoff.borrow().current_interval,
                Duration::from_secs(30)
            );
        }
    }

    /// STATEMENT: Exhausted backoff stops retries and returns the current creation or readiness error.
    ///
    /// HOW:
    /// - Fail creation or child readiness with a zero elapsed-time limit and retries remaining.
    /// - Verify readiness returns the original error after exactly one factory attempt.
    #[ntex::test]
    async fn exhausted_backoff_stops_retries() {
        struct FailReady;

        impl Service<(), ()> for FailReady {
            type Res = ();
            type Error = io::Error;

            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                Err(io::Error::other("readiness failed"))
            }

            async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), io::Error> {
                Ok(())
            }
        }

        for fail_creation in [true, false] {
            let attempts = Rc::new(Cell::new(0));
            let factory = fn_factory({
                let attempts = attempts.clone();
                move |(): &()| {
                    attempts.set(attempts.get() + 1);
                    async move {
                        if fail_creation {
                            Err(io::Error::other("creation failed"))
                        } else {
                            Ok(FailReady)
                        }
                    }
                }
            });
            let service = ReconnectService::new("exhausted-backoff", factory);
            service.backoff.borrow_mut().max_elapsed_time = Some(Duration::ZERO);
            let pipeline = Pipeline::new((), service);
            let err = pipeline.ready().await.unwrap_err();
            assert_eq!(
                err.to_string(),
                if fail_creation {
                    "creation failed"
                } else {
                    "readiness failed"
                }
            );
            assert_eq!(attempts.get(), 1, "creation error: {fail_creation}");
        }
    }

    /// STATEMENT: Configured retry budgets survive successful connection and recovery.
    ///
    /// HOW:
    /// - Set zero or five retries through `Reconnect` or `ReconnectService`.
    /// - Connect successfully, fail a call, and count replacement attempts while creation fails.
    /// - Recover and repeat, verifying each cycle permits exactly the configured extra attempts.
    #[ntex::test]
    async fn configured_retry_limit_survives_success() {
        for use_factory in [false, true] {
            for retry in [0, 5] {
                let attempts = Rc::new(Cell::new(0));
                let fail_creation = Rc::new(Cell::new(false));
                let factory = fn_factory({
                    let attempts = attempts.clone();
                    let fail_creation = fail_creation.clone();
                    move |(): &()| {
                        attempts.set(attempts.get() + 1);
                        let fail = fail_creation.get();
                        async move {
                            if fail {
                                Err(io::Error::other("creation failed"))
                            } else {
                                Ok(fn_service(|(): ()| async {
                                    Err::<(), _>(io::Error::other("call failed"))
                                }))
                            }
                        }
                    }
                });
                let service = if use_factory {
                    Reconnect::new("retry-limit", factory)
                        .retry(retry)
                        .create(&())
                        .await
                        .unwrap()
                } else {
                    ReconnectService::new("retry-limit", factory).retry(retry)
                };
                {
                    let mut backoff = service.backoff.borrow_mut();
                    backoff.initial_interval = Duration::ZERO;
                    backoff.max_elapsed_time = None;
                }
                let pipeline = Pipeline::new((), service);
                for _ in 0..2 {
                    fail_creation.set(false);
                    pipeline.ready().await.unwrap();
                    assert!(pipeline.call(()).await.is_err());

                    fail_creation.set(true);
                    let before = attempts.get();
                    assert!(pipeline.ready().await.is_err());
                    assert_eq!(
                        attempts.get() - before,
                        retry + 1,
                        "factory setter: {use_factory}, retry limit: {retry}"
                    );
                }
            }
        }
    }

    /// STATEMENT: Shutdown awaits the current child without creating or replacing a connection.
    ///
    /// HOW:
    /// - Exercise disconnected, connected, and call-failed reconnect services.
    /// - Wrap a child with a channel-gated shutdown in `ReadyBinding`.
    /// - Verify connected shutdown stays pending until the child finishes, even after a call error.
    /// - Verify disconnected shutdown skips the child and no shutdown path invokes the factory.
    #[ntex::test]
    async fn shutdown_awaits_current_child_without_reconnecting() {
        struct ShutdownProbe {
            started: Cell<usize>,
            finished: Cell<bool>,
            complete: oneshot::Receiver<()>,
        }

        impl Service<(), ()> for ShutdownProbe {
            type Res = ();
            type Error = io::Error;

            async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), io::Error> {
                Err(io::Error::other("call failed"))
            }

            async fn shutdown(&self, _: Ctx<'_, Self>) {
                self.started.set(self.started.get() + 1);
                self.complete.recv().await.unwrap();
                self.finished.set(true);
            }
        }

        for (connected, failed) in [(false, false), (true, false), (true, true)] {
            let created = Rc::new(Cell::new(0));
            let (complete, receiver) = oneshot::channel();
            let child = Rc::new(ShutdownProbe {
                started: Cell::new(0),
                finished: Cell::new(false),
                complete: receiver,
            });
            let factory = fn_factory({
                let created = created.clone();
                let child = child.clone();
                move |(): &()| {
                    created.set(created.get() + 1);
                    let child = child.clone();
                    async move { Ok::<_, io::Error>(ReadyBinding::new(child)) }
                }
            });
            let pipeline = Pipeline::new((), ReconnectService::new("shutdown", factory));
            if connected {
                pipeline.ready().await.unwrap();
            }
            if failed {
                assert!(pipeline.call(()).await.is_err());
            }

            let mut shutdown = pin!(pipeline.shutdown());
            let mut cx = Context::from_waker(std::task::Waker::noop());
            if connected {
                assert!(shutdown.as_mut().poll(&mut cx).is_pending());
                assert_eq!(child.started.get(), 1);
                assert!(!child.finished.get());
                complete.send(()).unwrap();
                shutdown.await;
                assert!(child.finished.get());
            } else {
                assert!(shutdown.as_mut().poll(&mut cx).is_ready());
                assert_eq!(child.started.get(), 0);
                assert!(!child.finished.get());
            }
            assert_eq!(created.get(), usize::from(connected));
        }
    }
}
