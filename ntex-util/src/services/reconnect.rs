//! Lazy service creation and policy-controlled reconnection.
//!
//! [`Reconnect`] creates independent [`ReconnectService`] instances. Each instance
//! owns one pipeline-local readiness scope and creates its child on demand.
//! Failed requests are returned to the caller, never replayed automatically.

use std::{
    cell::Cell, cell::RefCell, error::Error, fmt, marker::PhantomData, rc::Rc, time::Instant,
};

use backoff::{Clock, ExponentialBackoff, backoff::Backoff};
use ntex_error::fmt_err_string;
use ntex_service::{BindCtx, Ctx, ReadyBinding, ReadyNotifier, Service, ServiceFactory};

use crate::time;

/// Factory for independent, lazily connected [`ReconnectService`] instances.
///
/// Each [`ServiceFactory::create`] call returns a disconnected wrapper without
/// invoking the wrapped factory. The child is created when readiness is first
/// needed, using the state supplied by that operation's context.
///
/// The wrapped factory is shared through [`Rc`]; policy and retry settings are
/// copied into each service. The default policy permits reconnection after any
/// error, with a budget of three retries.
#[derive(Debug)]
pub struct Reconnect<P, Sf, St> {
    name: &'static str,
    policy: P,
    factory: Rc<Sf>,
    retry: usize,
    max_retry: usize,
    ph: PhantomData<St>,
}

/// Decides whether a service error permits retrying or replacing a connection.
///
/// Cloneable `Fn(&E) -> bool` closures implement this trait. Factory initialization
/// errors are converted to the service error type before the policy is consulted.
pub trait ReconnectPolicy<E>: Clone + Sized + 'static {
    /// Returns whether `err` permits reconnection.
    ///
    /// For creation or readiness errors, `true` permits another attempt if retry
    /// budget and backoff remain; `false` returns the error without retrying.
    /// A readiness-failed child is removed regardless of this decision.
    ///
    /// For a call error from the current child, `true` invalidates readiness and
    /// schedules replacement on the next readiness check; `false` keeps the
    /// child. The failed request is never replayed. Errors from retired children
    /// do not consult the policy or invalidate a replacement.
    fn reconnect(&self, err: &E) -> bool;
}

/// Policy that permits reconnection after every error.
///
/// Retry budgets and backoff still limit creation and readiness retries.
#[derive(Copy, Clone, Debug)]
pub struct DefaultPolicy;

impl<E> ReconnectPolicy<E> for DefaultPolicy {
    /// Permits reconnection without inspecting the error.
    fn reconnect(&self, _: &E) -> bool {
        true
    }
}

impl<T, E> ReconnectPolicy<E> for T
where
    T: Fn(&E) -> bool + Clone + 'static,
{
    /// Delegates the reconnection decision to this closure.
    fn reconnect(&self, err: &E) -> bool {
        (*self)(err)
    }
}

impl<Sf, St> Reconnect<DefaultPolicy, Sf, St>
where
    St: 'static,
    Sf: 'static,
{
    /// Wraps `factory` with [`DefaultPolicy`] and a budget of three retries.
    ///
    /// `name` identifies the service in diagnostic messages. Construction does
    /// not invoke the factory or establish a connection.
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
    /// Sets the number of additional attempts after creation or readiness errors.
    ///
    /// Zero permits the initial attempt but no retries. Each created service
    /// restores this budget after successful child readiness. A rejecting policy
    /// or exhausted backoff can stop retries before the budget is consumed.
    #[must_use]
    pub fn retry(mut self, retry: usize) -> Self {
        self.retry = retry;
        self.max_retry = retry;
        self
    }

    /// Replaces the error policy while preserving the factory and retry settings.
    ///
    /// The new policy is cloned into services created by this factory.
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
    /// Shares the wrapped factory and copies configuration without creating a child.
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

    /// Creates a disconnected service with independent readiness and retry state.
    ///
    /// The supplied state is not retained and the wrapped factory is not called.
    /// Child creation, including initialization errors, is deferred until readiness
    /// is needed and uses that operation's [`Ctx::st`].
    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(ReconnectService {
            inner: ReadyBinding::new(ReconnectServiceInner {
                name: self.name,
                policy: self.policy.clone(),
                factory: self.factory.clone(),
                service: Cell::new(None),
                err: Cell::new(false),
                changed: ReadyNotifier::new(),
                retry: Cell::new(self.retry),
                max_retry: self.max_retry,
                backoff: RefCell::new(ExponentialBackoff::default()),
                backoff_active: Cell::new(false),
                retry_at: Cell::new(None),
                ph: PhantomData,
            }),
        })
    }
}

/// Lazily creates a child service and replaces it after policy-approved errors.
///
/// Creation and readiness failures can retry within the configured budget using
/// exponential backoff. Successful creation resets backoff, even if the new
/// child's readiness later fails; successful readiness restores the retry budget.
/// Unused and healthy time do not count toward the backoff time limit. Once
/// recovery starts, its elapsed-time window and retry deadlines survive cancellation.
///
/// Call errors are returned without replaying the request. If the policy permits
/// reconnection, an error from the current child invalidates readiness so a later
/// operation can replace it. Shutdown awaits the stored child without reconnecting.
///
/// Each child has its own readiness scope. Retired children's callbacks are
/// released and their notifier subscriptions become inert when the last child
/// handle is dropped, including handles held by outstanding calls.
///
/// Clones start disconnected and create their own inner service on the next
/// readiness check. They copy the policy, remaining and configured retry budgets,
/// and backoff settings and interval, but not the connection, elapsed-time window,
/// or a pending retry deadline.
///
/// Each instance belongs to the first pipeline that binds it. Sharing an
/// `Rc<ReconnectService>` within that pipeline shares its connection and readiness;
/// binding the same instance to another pipeline panics, even after the owner
/// is dropped. Clone the service itself for independent connections, or share
/// bindings to one [`ntex_service::Pipeline`] for a shared connection.
///
/// # Examples
///
/// ```
/// use std::io;
/// use ntex_service::{Pipeline, fn_factory, fn_service};
/// use ntex_util::services::reconnect::ReconnectService;
///
/// # async fn example() -> Result<(), io::Error> {
/// let factory = fn_factory(|_: &()| async {
///     Ok::<_, io::Error>(fn_service(|req: u32| async move {
///         Ok::<_, io::Error>(req)
///     }))
/// });
/// let service = ReconnectService::new("upstream", factory).retry(2);
/// let first = Pipeline::new((), service.clone());
/// let second = Pipeline::new((), service);
///
/// assert_eq!(first.call(1).await?, 1);
/// assert_eq!(second.call(2).await?, 2);
/// # Ok(())
/// # }
/// ```
pub struct ReconnectService<P, Sf, S, St> {
    inner: ReadyBinding<ReconnectServiceInner<P, Sf, S, St>>,
}

/// Connection and retry state driven inside the wrapper's owned readiness scope.
///
/// The enclosing [`ReadyBinding`] serializes readiness checks and propagates child
/// invalidations to every parent within the owning pipeline. Each child binding
/// keeps its readiness registrations separate from this long-lived scope.
struct ReconnectServiceInner<P, Sf, S, St> {
    name: &'static str,
    policy: P,
    factory: Rc<Sf>,
    service: Cell<Option<Rc<ReadyBinding<S>>>>,
    err: Cell<bool>,
    changed: ReadyNotifier,
    retry: Cell<usize>,
    max_retry: usize,
    backoff: RefCell<ExponentialBackoff>,
    backoff_active: Cell<bool>,
    // Keep the retry deadline when a readiness check is cancelled.
    retry_at: Cell<Option<Instant>>,
    ph: PhantomData<St>,
}

impl<P, Sf, S, St> fmt::Debug for ReconnectService<P, Sf, S, St> {
    /// Formats connection metadata without requiring the factory or policy to implement `Debug`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.get_ref().fmt(f)
    }
}

impl<P: Clone, Sf, S, St> Clone for ReconnectService<P, Sf, S, St> {
    /// Creates a disconnected clone with a fresh, unbound readiness scope.
    ///
    /// The factory is shared, but the original connection and pipeline ownership
    /// are not. No child is created until the clone needs readiness.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<P, Sf, S, St> fmt::Debug for ReconnectServiceInner<P, Sf, S, St> {
    /// Formats type names, error state, retry counters, backoff activity, and the pending deadline.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconnectService")
            .field("name", &self.name)
            .field("policy", &std::any::type_name::<P>())
            .field("factory", &std::any::type_name::<Sf>())
            .field("service", &std::any::type_name::<S>())
            .field("err", &self.err)
            .field("retry", &self.retry)
            .field("max_retry", &self.max_retry)
            .field("backoff_active", &self.backoff_active)
            .field("retry_at", &self.retry_at)
            .finish()
    }
}

impl<P: Clone, Sf, S, St> Clone for ReconnectServiceInner<P, Sf, S, St> {
    /// Copies policy, retry counters, and backoff without sharing a child or notifier.
    ///
    /// The clone starts without an error flag, active backoff window, or pending retry deadline.
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
            backoff_active: Cell::new(false),
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
    /// Creates a disconnected service with [`DefaultPolicy`] and three retries.
    ///
    /// `name` is used in diagnostics. The factory is retained without being called;
    /// binding to a pipeline is also lazy with respect to child creation.
    pub fn new<Req>(name: &'static str, factory: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Service = S, Error = S::Error> + 'static,
        Sf::Error: Error,
        Sf::InitError: Into<Sf::Error> + Error,
        S: Service<St, Req>,
    {
        Self {
            inner: ReadyBinding::new(ReconnectServiceInner {
                name,
                policy: DefaultPolicy,
                factory: Rc::new(factory),
                service: Cell::new(None),
                err: Cell::new(false),
                changed: ReadyNotifier::new(),
                retry: Cell::new(3),
                max_retry: 3,
                backoff: RefCell::new(ExponentialBackoff::default()),
                backoff_active: Cell::new(false),
                retry_at: Cell::new(None),
                ph: PhantomData,
            }),
        }
    }
}

impl<P, Sf, S, St> ReconnectService<P, Sf, S, St> {
    /// Sets both the remaining and configured budget for additional attempts.
    ///
    /// Zero permits the initial attempt but no retries after creation or readiness
    /// errors. Successful child readiness restores this budget; policy rejection
    /// or backoff exhaustion can stop retries earlier.
    ///
    /// Preserves the current child, pending deadline, backoff, and pipeline owner.
    #[must_use]
    pub fn retry(self, retry: usize) -> Self {
        Self {
            inner: self.inner.map(|mut inner| {
                inner.retry.set(retry);
                inner.max_retry = retry;
                inner
            }),
        }
    }

    /// Replaces the error policy without reconnecting or resetting readiness.
    ///
    /// The connection, retry state, backoff, pending deadline, and pipeline owner
    /// are preserved. Changing policy does not undo an already scheduled retry.
    pub fn policy<T, Req>(self, policy: T) -> ReconnectService<T, Sf, S, St>
    where
        T: ReconnectPolicy<Sf::Error>,
        Sf: ServiceFactory<St, Req>,
    {
        ReconnectService {
            inner: self.inner.map(|inner| ReconnectServiceInner {
                policy,
                name: inner.name,
                factory: inner.factory,
                service: inner.service,
                err: inner.err,
                changed: inner.changed,
                retry: inner.retry,
                max_retry: inner.max_retry,
                backoff: inner.backoff,
                backoff_active: inner.backoff_active,
                retry_at: inner.retry_at,
                ph: PhantomData,
            }),
        }
    }
}

impl<P, Sf, S, St> ReconnectServiceInner<P, Sf, S, St> {
    /// Clones the stored child handle without removing it or checking readiness.
    ///
    /// Includes a child marked errored so shutdown can still reach it.
    fn current(&self) -> Option<Rc<ReadyBinding<S>>> {
        let svc = self.service.take();
        self.service.set(svc.clone());
        svc
    }

    /// Removes `svc` only if it is still the current child, preserving any replacement.
    ///
    /// Other handles keep a retired child and its readiness scope alive until they
    /// are dropped. This does not invoke the child's shutdown.
    fn remove(&self, svc: &Rc<ReadyBinding<S>>) {
        if let Some(current) = self.service.take()
            && !Rc::ptr_eq(&current, svc)
        {
            self.service.set(Some(current));
        }
    }

    /// Starts elapsed-time accounting once per recovery, preserving the retry interval.
    ///
    /// Pending and failed attempts retain this window until child readiness succeeds.
    fn start_backoff(&self) {
        if !self.backoff_active.replace(true) {
            let mut backoff = self.backoff.borrow_mut();
            backoff.start_time = backoff.clock.now();
        }
    }

    /// Returns a ready child, replacing an errored or missing connection as needed.
    ///
    /// Waits for any retained retry deadline before creating a child wrapped in
    /// [`ReadyBinding`] through [`Ctx::create`], so binding uses the child's own
    /// scope. The child stays stored while readiness is checked.
    /// Creation success resets backoff and clears the deadline; readiness success
    /// restores the configured retry budget and ends the recovery window.
    ///
    /// # Errors
    ///
    /// Returns the current creation or readiness error when the policy rejects a
    /// retry, the retry budget is exhausted, or backoff produces no further delay.
    /// Creation errors are converted to the service error type.
    ///
    /// # Cancellation
    ///
    /// The remaining retry budget, recovery window, and absolute deadline are stored
    /// on the service, so restarting a cancelled check does not renew the time limit
    /// or skip an outstanding delay.
    async fn ensure<Req>(&self, ctx: Ctx<'_, Self, St>) -> Result<Rc<ReadyBinding<S>>, S::Error>
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
                self.start_backoff();
                let factory = ntex_service::fn_factory(async |st: &St| {
                    self.factory.create(st).await.map(ReadyBinding::new)
                });
                match ctx.create(&factory).await {
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
                self.start_backoff();

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
                self.backoff_active.set(false);
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

    /// Registers the owned readiness scope without creating a child.
    ///
    /// Repeated binding within the same pipeline shares this scope.
    ///
    /// # Panics
    ///
    /// Panics if this instance was already bound to another pipeline, even if
    /// that pipeline has been dropped.
    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.bind(&self.inner);
    }

    /// Waits for the owned readiness gate, lazily connecting or recovering as needed.
    ///
    /// Successful readiness is cached until a subscribed notification invalidates
    /// it. Returns creation or readiness errors that cannot be retried.
    ///
    /// # Panics
    ///
    /// Panics if the caller's scope has not bound this instance.
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        ctx.ready(&self.inner).await
    }

    /// Dispatches through the owned readiness gate without replaying failed requests.
    ///
    /// Returns readiness or child call errors. A policy-approved call error marks
    /// the current connection for replacement by a later operation.
    ///
    /// # Panics
    ///
    /// Panics if the caller's scope has not bound this instance.
    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Self::Res, Self::Error> {
        ctx.call(&self.inner, req).await
    }

    /// Awaits shutdown of the stored child, or completes immediately if disconnected.
    ///
    /// Does not check readiness, create a child, or reconnect an errored child.
    ///
    /// # Panics
    ///
    /// Panics if the caller's scope has not bound this instance.
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        ctx.shutdown(&self.inner).await;
    }
}

impl<P, Sf, S, St, Req> Service<St, Req> for ReconnectServiceInner<P, Sf, S, St>
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

    /// Subscribes the owned scope to reconnect changes and binds any existing child.
    ///
    /// Missing children are bound later by [`Ctx::create`], not created here.
    fn bind(&self, ctx: BindCtx<'_>) {
        ctx.subscribe(&self.changed);
        if let Some(svc) = self.current() {
            ctx.bind(svc.as_ref());
        }
    }

    /// Ensures a ready child, returning any terminal error from the current attempt.
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        self.ensure(ctx).await.map(|_| ())
    }

    /// Shuts down the stored child, including one marked errored, without replacement.
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        if let Some(svc) = self.current() {
            ctx.shutdown(svc.as_ref()).await;
        }
    }

    /// Uses the current child or restores one, then dispatches the request once.
    ///
    /// Only a policy-approved error from the still-current child invalidates
    /// readiness. Late errors from retired children are returned unchanged.
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

    use ntex_service::{Pipeline, ReadyBinding, ReadyCallback, fn_factory, fn_service};

    use super::*;
    use crate::channel::oneshot;

    struct Probe {
        id: usize,
        ready: Cell<bool>,
        fail: Cell<bool>,
        changed: ReadyNotifier,
    }

    impl Service<(), ()> for Probe {
        type Res = usize;
        type Error = io::Error;

        /// Subscribes the caller to this probe's availability changes.
        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.changed);
        }

        /// Waits until the test marks the probe ready.
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

        /// Returns the child identity or the call failure configured by the test.
        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<usize, io::Error> {
            if self.fail.get() {
                Err(io::Error::other("call failed"))
            } else {
                Ok(self.id)
            }
        }
    }

    struct Branches<S> {
        first: ReadyBinding<S>,
        second: ReadyBinding<S>,
    }

    impl<S> Service<(), bool> for Branches<S>
    where
        S: Service<(), (), Res = usize, Error = io::Error>,
    {
        type Res = usize;
        type Error = io::Error;

        /// Registers both branch caches as dependencies of the enclosing scope.
        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.bind(&self.first);
            ctx.bind(&self.second);
        }

        /// Selects a branch and dispatches through its independent readiness cache.
        async fn call(&self, second: bool, ctx: Ctx<'_, Self>) -> Result<usize, io::Error> {
            if second {
                ctx.call(&self.second, ()).await
            } else {
                ctx.call(&self.first, ()).await
            }
        }
    }

    /// STATEMENT: Reconnect diagnostics delegate to the inner state without requiring child or policy `Debug`.
    ///
    /// HOW:
    /// - Create a lazy wrapper with a closure policy and a non-`Debug` child.
    /// - Compare wrapper and inner formatting, then change retry state and check the reported fields.
    /// - Verify formatting never initializes the child.
    #[ntex::test]
    async fn debug_reconnect_state() {
        let service = Reconnect::new(
            "debug-reconnect",
            fn_factory(async |(): &()| {
                Ok::<_, io::Error>(Probe {
                    id: 1,
                    ready: Cell::new(true),
                    fail: Cell::new(false),
                    changed: ReadyNotifier::new(),
                })
            }),
        )
        .retry(7)
        .policy(|_: &io::Error| true)
        .create(&())
        .await
        .unwrap();
        let inner = service.inner.get_ref();
        let initial = format!("{service:?}");
        assert_eq!(initial, format!("{inner:?}"));
        assert!(initial.starts_with("ReconnectService { name: \"debug-reconnect\","));
        assert!(initial.contains(std::any::type_name::<Probe>()));
        assert!(initial.contains(std::any::type_name_of_val(&inner.policy)));
        assert!(initial.contains(std::any::type_name_of_val(inner.factory.as_ref())));
        assert!(initial.contains("err: Cell { value: false }"));
        assert!(initial.contains("retry: Cell { value: 7 }, max_retry: 7"));
        assert!(initial.contains("backoff_active: Cell { value: false }"));
        assert!(initial.contains("retry_at: Cell { value: None }"));

        inner.err.set(true);
        inner.retry.set(6);
        inner.backoff_active.set(true);
        inner.retry_at.set(Some(Instant::now()));
        let retrying = format!("{service:?}");
        assert_eq!(retrying, format!("{inner:?}"));
        assert!(retrying.contains("err: Cell { value: true }"));
        assert!(retrying.contains("retry: Cell { value: 6 }, max_retry: 7"));
        assert!(retrying.contains("backoff_active: Cell { value: true }"));
        assert!(retrying.contains("retry_at: Cell { value: Some("));
        assert!(inner.service.take().is_none());
    }

    /// STATEMENT: Cloned reconnect factories preserve policy and retries while deferring child state.
    ///
    /// HOW:
    /// - Clone a factory with one retry and a policy inspecting converted initialization errors.
    /// - Create disconnected wrappers with different state from their eventual pipelines.
    /// - Verify retryable errors get two attempts and rejected errors get only one.
    /// - Recover both pipelines and verify their independent children use the pipeline state.
    #[ntex::test]
    async fn factory_clones_preserve_configuration_and_defer_state() {
        #[derive(Debug)]
        struct InitError(io::ErrorKind);

        impl fmt::Display for InitError {
            /// Formats the factory-specific initialization error.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "initialization failed: {:?}", self.0)
            }
        }

        impl Error for InitError {}

        impl From<InitError> for io::Error {
            /// Preserves the error kind when converting to the child's error type.
            fn from(err: InitError) -> Self {
                err.0.into()
            }
        }

        let attempts = Rc::new(RefCell::new(Vec::new()));
        let failure = Rc::new(Cell::new(Some(io::ErrorKind::Interrupted)));
        let decisions = Rc::new(RefCell::new(Vec::new()));
        let factory = fn_factory({
            let attempts = attempts.clone();
            let failure = failure.clone();
            move |state: &usize| {
                let state = *state;
                attempts.borrow_mut().push(state);
                let failure = failure.get();
                async move {
                    if let Some(kind) = failure {
                        Err(InitError(kind))
                    } else {
                        Ok(fn_service(move |(): ()| async move {
                            Ok::<_, io::Error>(state)
                        }))
                    }
                }
            }
        });
        let factory = Reconnect::new("factory-clone", factory).retry(1).policy({
            let decisions = decisions.clone();
            move |err: &io::Error| {
                decisions.borrow_mut().push(err.kind());
                err.kind() == io::ErrorKind::Interrupted
            }
        });
        let cloned = factory.clone();
        let first = factory.create(&100).await.unwrap();
        let second = cloned.create(&200).await.unwrap();
        for service in [&first, &second] {
            let mut backoff = service.inner.get_ref().backoff.borrow_mut();
            backoff.initial_interval = Duration::ZERO;
            backoff.reset();
        }
        let first = Pipeline::new(1, first);
        let second = Pipeline::new(2, second);
        assert!(attempts.borrow().is_empty());

        assert_eq!(
            first.ready().await.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(*attempts.borrow(), [1, 1]);
        failure.set(Some(io::ErrorKind::PermissionDenied));
        assert_eq!(
            second.ready().await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(*attempts.borrow(), [1, 1, 2]);
        failure.set(Some(io::ErrorKind::Interrupted));
        assert_eq!(
            second.ready().await.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(*attempts.borrow(), [1, 1, 2, 2, 2]);
        assert_eq!(
            *decisions.borrow(),
            [
                io::ErrorKind::Interrupted,
                io::ErrorKind::Interrupted,
                io::ErrorKind::PermissionDenied,
                io::ErrorKind::Interrupted,
                io::ErrorKind::Interrupted,
            ]
        );

        failure.set(None);
        assert_eq!(first.call(()).await.unwrap(), 1);
        assert_eq!(second.call(()).await.unwrap(), 2);
        assert_eq!(first.call(()).await.unwrap(), 1);
        assert_eq!(*attempts.borrow(), [1, 1, 2, 2, 2, 1, 2]);
    }

    /// STATEMENT: Rejecting a readiness retry still removes the failed child.
    ///
    /// HOW:
    /// - Fail the first child's readiness with retries available and a rejecting policy.
    /// - Verify the error reaches the caller without another attempt and the child is dropped.
    /// - Call again and verify a fresh, healthy child serves the request.
    #[ntex::test]
    async fn rejected_readiness_retry_discards_child() {
        struct RejectReady(usize);

        impl Service<(), ()> for RejectReady {
            type Res = usize;
            type Error = io::Error;

            /// Rejects readiness only for the first child.
            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                if self.0 == 0 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            }

            /// Returns the identity of the child that received the request.
            async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<usize, io::Error> {
                Ok(self.0)
            }
        }

        let children = Rc::new(RefCell::new(Vec::new()));
        let decisions = Rc::new(Cell::new(0));
        let factory = fn_factory({
            let children = children.clone();
            move |(): &()| {
                let id = children.borrow().len();
                let child = Rc::new(RejectReady(id));
                children.borrow_mut().push(Rc::downgrade(&child));
                async move { Ok::<_, io::Error>(child) }
            }
        });
        let service = ReconnectService::new("reject-readiness", factory).policy({
            let decisions = decisions.clone();
            move |err: &io::Error| {
                assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
                decisions.set(decisions.get() + 1);
                false
            }
        });
        let pipeline = Pipeline::new((), service);
        assert_eq!(
            pipeline.call(()).await.unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(children.borrow().len(), 1);
        assert!(children.borrow()[0].upgrade().is_none());
        assert_eq!(decisions.get(), 1);
        assert_eq!(pipeline.call(()).await.unwrap(), 1);
        assert_eq!(children.borrow().len(), 2);
        assert!(children.borrow()[1].upgrade().is_some());
        assert_eq!(decisions.get(), 1);
    }

    /// STATEMENT: Call policy rejection retains the child, and approved failures are never replayed.
    ///
    /// HOW:
    /// - Reject reconnection after a call error and verify readiness and child identity are retained.
    /// - Allow reconnection after another error and verify readiness is invalidated.
    /// - Record every child call to verify failures are not replayed, then call the replacement.
    #[ntex::test]
    async fn call_policy_controls_replacement_without_replaying() {
        let created = Rc::new(Cell::new(0));
        let calls = Rc::new(RefCell::new(Vec::new()));
        let allow = Rc::new(Cell::new(false));
        let decisions = Rc::new(Cell::new(0));
        let factory = fn_factory({
            let created = created.clone();
            let calls = calls.clone();
            move |(): &()| {
                let id = created.get();
                created.set(id + 1);
                let calls = calls.clone();
                async move {
                    Ok::<_, io::Error>(fn_service(move |fail: bool| {
                        calls.borrow_mut().push((id, fail));
                        async move {
                            if fail {
                                Err(io::ErrorKind::BrokenPipe.into())
                            } else {
                                Ok(id)
                            }
                        }
                    }))
                }
            }
        });
        let service = ReconnectService::new("call-policy", factory).policy({
            let allow = allow.clone();
            let decisions = decisions.clone();
            move |err: &io::Error| {
                assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
                decisions.set(decisions.get() + 1);
                allow.get()
            }
        });
        let pipeline = Pipeline::new((), service);
        assert_eq!(pipeline.call(false).await.unwrap(), 0);
        assert_eq!(
            pipeline.call(true).await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(pipeline.is_ready());
        assert_eq!(pipeline.call(false).await.unwrap(), 0);
        assert_eq!(created.get(), 1);
        assert_eq!(decisions.get(), 1);

        allow.set(true);
        assert!(pipeline.call(true).await.is_err());
        assert!(!pipeline.is_ready());
        assert_eq!(created.get(), 1);
        assert_eq!(
            *calls.borrow(),
            [(0, false), (0, true), (0, false), (0, true)]
        );
        assert_eq!(pipeline.call(false).await.unwrap(), 1);
        assert_eq!(
            *calls.borrow(),
            [(0, false), (0, true), (0, false), (0, true), (1, false)]
        );
        assert_eq!(created.get(), 2);
        assert_eq!(decisions.get(), 2);
    }

    /// STATEMENT: A reconnect instance rejects other pipelines without losing its original owner.
    ///
    /// HOW:
    /// - Bind an `Rc<ReconnectService>` before or after connecting and reject a second pipeline.
    /// - Verify the original pipeline still works and rejection does not invoke the factory.
    /// - Drop the owner, change retry and policy settings, and verify ownership is not transferred.
    #[ntex::test]
    async fn shared_service_rejects_another_pipeline() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        for connected in [false, true] {
            let created = Rc::new(Cell::new(0));
            let factory = fn_factory({
                let created = created.clone();
                move |(): &()| {
                    created.set(created.get() + 1);
                    async {
                        Ok::<_, io::Error>(fn_service(|(): ()| async { Ok::<_, io::Error>(()) }))
                    }
                }
            });
            let service = Rc::new(ReconnectService::new("single-owner", factory));
            let original = Pipeline::new((), service.clone());
            if connected {
                original.ready().await.unwrap();
            }
            assert!(catch_unwind(AssertUnwindSafe(|| Pipeline::new((), service.clone()))).is_err());
            assert_eq!(created.get(), usize::from(connected));
            original.call(()).await.unwrap();
            assert_eq!(created.get(), 1);

            drop(original);
            let service = Rc::try_unwrap(service)
                .unwrap()
                .retry(5)
                .policy(|_: &io::Error| true);
            assert!(catch_unwind(AssertUnwindSafe(|| Pipeline::new((), service))).is_err());
            assert_eq!(created.get(), 1);
        }
    }

    /// STATEMENT: Parents within one pipeline share reconnect creation and child invalidations.
    ///
    /// HOW:
    /// - Share a reconnect instance between two call-only branches with separate readiness caches.
    /// - Gate initial creation and verify concurrent branch calls create only one child.
    /// - Exercise raw and `ReadyBinding` children, blocking readiness after both branches are cached.
    /// - Verify the second branch waits, then repeat after a call error replaces the child.
    #[ntex::test]
    async fn same_pipeline_parents_share_reconnect_scope() {
        for bound_child in [false, true] {
            let created = Rc::new(Cell::new(0));
            let children = Rc::new(RefCell::new(Vec::new()));
            let (release, gate) = oneshot::channel();
            let gate = Rc::new(gate);
            let factory = fn_factory({
                let created = created.clone();
                let children = children.clone();
                move |(): &()| {
                    let id = created.get();
                    created.set(id + 1);
                    let children = children.clone();
                    let gate = gate.clone();
                    async move {
                        if id == 0 {
                            gate.recv().await.unwrap();
                        }
                        let child = Rc::new(Probe {
                            id,
                            ready: Cell::new(true),
                            fail: Cell::new(false),
                            changed: ReadyNotifier::new(),
                        });
                        children.borrow_mut().push(child.clone());
                        Ok::<_, io::Error>(if bound_child {
                            ntex_service::boxed::service(ReadyBinding::new(child))
                        } else {
                            ntex_service::boxed::service(child)
                        })
                    }
                }
            });
            let service = Rc::new(ReconnectService::new("shared-scope", factory));
            let pipeline = Pipeline::new(
                (),
                Branches {
                    first: ReadyBinding::new(service.clone()),
                    second: ReadyBinding::new(service),
                },
            );
            assert_eq!(created.get(), 0);
            let mut cx = Context::from_waker(std::task::Waker::noop());
            {
                let mut first = pin!(pipeline.call(false));
                let mut second = pin!(pipeline.call(true));
                assert!(first.as_mut().poll(&mut cx).is_pending());
                assert!(second.as_mut().poll(&mut cx).is_pending());
                assert_eq!(created.get(), 1);
                release.send(()).unwrap();
                assert_eq!(first.await.unwrap(), 0);
                assert_eq!(second.await.unwrap(), 0);
            }

            for id in 0..2 {
                assert_eq!(pipeline.call(false).await.unwrap(), id);
                assert_eq!(pipeline.call(true).await.unwrap(), id);
                let child = children.borrow()[id].clone();
                child.ready.set(false);
                child.changed.notify();
                assert!(!pipeline.is_ready());
                let mut call = pin!(pipeline.call(true));
                assert!(call.as_mut().poll(&mut cx).is_pending());
                child.ready.set(true);
                child.changed.notify();
                assert_eq!(call.await.unwrap(), id);

                if id == 0 {
                    child.fail.set(true);
                    assert!(pipeline.call(false).await.is_err());
                }
            }
            assert_eq!(created.get(), 2);
        }
    }

    /// STATEMENT: Binding a connected reconnect service to a new parent reuses its live child.
    ///
    /// HOW:
    /// - Connect through one parent, then create another bound parent in the same pipeline.
    /// - Make the existing child unavailable and verify a newly bound parent's call waits.
    /// - Restore readiness and verify both paths still use the original child without recreation.
    #[ntex::test]
    async fn late_parent_binding_reuses_connected_child() {
        struct Rebind<S>(Rc<S>);

        impl<S> Service<(), bool> for Rebind<S>
        where
            S: Service<(), (), Res = usize, Error = io::Error>,
        {
            type Res = usize;
            type Error = io::Error;

            /// Registers the original parent before the reconnect service has a child.
            fn bind(&self, ctx: BindCtx<'_>) {
                ctx.bind(self.0.as_ref());
            }

            /// Optionally creates a new parent binding before forwarding the call.
            async fn call(&self, rebind: bool, ctx: Ctx<'_, Self>) -> Result<usize, io::Error> {
                if rebind {
                    let factory = fn_factory(async |(): &()| {
                        Ok::<_, io::Error>(ReadyBinding::new(self.0.clone()))
                    });
                    let parent = ctx.create(&factory).await?;
                    ctx.call(&parent, ()).await
                } else {
                    ctx.call(self.0.as_ref(), ()).await
                }
            }
        }

        let created = Rc::new(Cell::new(0));
        let child = Rc::new(Probe {
            id: 0,
            ready: Cell::new(true),
            fail: Cell::new(false),
            changed: ReadyNotifier::new(),
        });
        let factory = fn_factory({
            let created = created.clone();
            let child = child.clone();
            move |(): &()| {
                created.set(created.get() + 1);
                let child = child.clone();
                async move { Ok::<_, io::Error>(child) }
            }
        });
        let service = Rc::new(ReconnectService::new("late-parent", factory));
        let pipeline = Pipeline::new((), Rebind(service));
        assert_eq!(pipeline.call(false).await.unwrap(), 0);
        assert_eq!(pipeline.call(true).await.unwrap(), 0);

        child.ready.set(false);
        child.changed.notify();
        let mut call = pin!(pipeline.call(true));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(call.as_mut().poll(&mut cx).is_pending());
        assert_eq!(created.get(), 1);
        child.ready.set(true);
        child.changed.notify();
        assert_eq!(call.await.unwrap(), 0);
        assert_eq!(pipeline.call(false).await.unwrap(), 0);
        assert_eq!(created.get(), 1);
    }

    /// STATEMENT: Retired children release readiness registrations after their last call completes.
    ///
    /// HOW:
    /// - Register a raw child's callback with a weakly observed resource and retain its notifier.
    /// - Replace the child while holding a call, then verify the resource outlives that call only.
    /// - Repeat replacement and verify retired notifiers do not invalidate the current child.
    /// - Fail the current child's readiness and verify its callback resource is also released.
    #[ntex::test]
    async fn retired_children_release_registrations() {
        struct ScopedProbe {
            id: usize,
            fail_ready: Rc<Cell<bool>>,
            changed: ReadyNotifier,
            callback: ReadyCallback,
        }

        impl Service<(), (bool, Option<oneshot::Receiver<()>>)> for ScopedProbe {
            type Res = usize;
            type Error = io::Error;

            /// Registers this child's notification source and resource-owning callback.
            fn bind(&self, ctx: BindCtx<'_>) {
                ctx.subscribe(&self.changed);
                ctx.on_notify(&self.callback);
            }

            /// Fails readiness when the test retires the final child.
            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                if self.fail_ready.get() {
                    Err(io::Error::other("readiness failed"))
                } else {
                    Ok(())
                }
            }

            /// Holds an optional in-flight call, then returns its identity or a replacement trigger.
            async fn call(
                &self,
                (fail, gate): (bool, Option<oneshot::Receiver<()>>),
                _: Ctx<'_, Self>,
            ) -> Result<usize, io::Error> {
                if let Some(gate) = gate {
                    gate.await.unwrap();
                }
                if fail {
                    Err(io::Error::other("call failed"))
                } else {
                    Ok(self.id)
                }
            }
        }

        let registrations = Rc::new(RefCell::new(Vec::new()));
        let fail_ready = Rc::new(Cell::new(false));
        let factory = fn_factory({
            let registrations = registrations.clone();
            let fail_ready = fail_ready.clone();
            move |(): &()| {
                let id = registrations.borrow().len();
                let changed = ReadyNotifier::new();
                let notifications = Rc::new(Cell::new(0));
                registrations
                    .borrow_mut()
                    .push((changed.clone(), Rc::downgrade(&notifications)));
                let fail_ready = fail_ready.clone();
                async move {
                    Ok::<_, io::Error>(ScopedProbe {
                        id,
                        fail_ready,
                        changed,
                        callback: ReadyCallback::new(move |_| {
                            notifications.set(notifications.get() + 1);
                        }),
                    })
                }
            }
        });
        let pipeline = Pipeline::new(
            (),
            ReconnectService::new("child-registrations", factory).retry(0),
        );
        let (release, gate) = oneshot::channel();
        let mut held = pin!(pipeline.call((false, Some(gate))));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(held.as_mut().poll(&mut cx).is_pending());
        assert!(pipeline.call((true, None)).await.is_err());
        assert_eq!(pipeline.call((false, None)).await.unwrap(), 1);
        assert!(registrations.borrow()[0].1.upgrade().is_some());

        release.send(()).unwrap();
        assert_eq!(held.await.unwrap(), 0);
        assert!(registrations.borrow()[0].1.upgrade().is_none());
        registrations.borrow()[0].0.notify();
        assert!(pipeline.is_ready());

        for id in 1..4 {
            assert!(pipeline.call((true, None)).await.is_err());
            assert_eq!(pipeline.call((false, None)).await.unwrap(), id + 1);
            assert!(registrations.borrow()[id].1.upgrade().is_none());
            registrations.borrow()[id].0.notify();
            assert!(pipeline.is_ready());
        }
        fail_ready.set(true);
        registrations.borrow()[4].0.notify();
        assert_eq!(registrations.borrow()[4].1.upgrade().unwrap().get(), 1);
        assert!(!pipeline.is_ready());
        assert_eq!(
            pipeline.ready().await.unwrap_err().to_string(),
            "readiness failed"
        );
        assert!(registrations.borrow()[4].1.upgrade().is_none());
    }

    /// STATEMENT: Retired child notifications cannot cancel replacement creation.
    ///
    /// HOW:
    /// - Fail a raw child and suspend creation of its replacement behind a channel.
    /// - Notify the retired source and verify the factory is not restarted.
    /// - Complete creation and verify the retired source no longer invalidates readiness.
    #[ntex::test]
    async fn retired_notifications_preserve_replacement_creation() {
        let attempts = Rc::new(Cell::new(0));
        let retired_source = ReadyNotifier::new();
        let (release, gate) = oneshot::channel();
        let gate = Rc::new(gate);
        let factory = fn_factory({
            let attempts = attempts.clone();
            let retired_source = retired_source.clone();
            move |(): &()| {
                let id = attempts.get();
                attempts.set(id + 1);
                let gate = gate.clone();
                let changed = if id == 0 {
                    retired_source.clone()
                } else {
                    ReadyNotifier::new()
                };
                async move {
                    if id > 0 {
                        gate.recv().await.unwrap();
                    }
                    Ok::<_, io::Error>(Probe {
                        id,
                        ready: Cell::new(true),
                        fail: Cell::new(id == 0),
                        changed,
                    })
                }
            }
        });
        let pipeline = Pipeline::new((), ReconnectService::new("retired-source", factory));
        assert!(pipeline.call(()).await.is_err());
        assert_eq!(attempts.get(), 1);
        let mut ready = pin!(pipeline.ready());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        assert_eq!(attempts.get(), 2);
        retired_source.notify();
        assert!(ready.as_mut().poll(&mut cx).is_pending());
        assert_eq!(attempts.get(), 2);

        release.send(()).unwrap();
        ready.await.unwrap();
        assert_eq!(pipeline.call(()).await.unwrap(), 1);
        retired_source.notify();
        assert!(pipeline.is_ready());
        assert_eq!(attempts.get(), 2);
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
                        fail: Cell::new(false),
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
    /// - Verify the error is returned without consulting the policy again, invalidating readiness,
    ///   or creating another child.
    #[ntex::test]
    async fn late_call_error_preserves_replacement() {
        let created = Rc::new(Cell::new(0));
        let decisions = Rc::new(Cell::new(0));
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
        let service = ReconnectService::new("late-error", factory).policy({
            let decisions = decisions.clone();
            move |_: &io::Error| {
                decisions.set(decisions.get() + 1);
                true
            }
        });
        let pipeline = Pipeline::new((), service);
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
        assert_eq!(decisions.get(), 1);

        late_tx.send(()).unwrap();
        assert!(late_call.await.is_err());
        assert_eq!(decisions.get(), 1);
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

            /// Fails readiness until the test clears the shared failure flag.
            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                if self.0.get() {
                    Err(io::Error::other("readiness failed"))
                } else {
                    Ok(())
                }
            }

            /// Completes a request after readiness recovery.
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
            let inner = service.inner.get_ref();
            {
                let mut backoff = inner.backoff.borrow_mut();
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
                inner.changed.notify();
                assert!(ready.as_mut().poll(&mut cx).is_pending());
                assert_eq!(created.get(), 1);
            }
            let retry_at = inner.retry_at.get();
            failing.set(false);
            {
                let mut ready = pin!(pipeline.ready());
                assert!(ready.as_mut().poll(&mut cx).is_pending());
                assert_eq!(created.get(), 1);
            }
            assert_eq!(inner.retry_at.get(), retry_at);

            let cloned = Pipeline::new((), service.as_ref().clone());
            {
                let mut ready = pin!(cloned.ready());
                assert!(matches!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
            }
            assert_eq!(created.get(), 2);

            inner
                .retry_at
                .set(Some(Instant::now() + Duration::from_millis(1)));
            pipeline.ready().await.unwrap();
            pipeline.call(()).await.unwrap();
            assert_eq!(created.get(), 3);
            assert_eq!(
                inner.backoff.borrow().current_interval,
                Duration::from_secs(30)
            );
        }
    }

    /// STATEMENT: Creation resets backoff before readiness succeeds, but preserves the retry budget.
    ///
    /// HOW:
    /// - Fail initial creation to grow backoff and consume one of two retries.
    /// - Expire the elapsed-time window, then create a replacement whose readiness is gated.
    /// - Verify creation reset the interval and elapsed clock without restoring the retry budget.
    /// - Fail child readiness, consume the final retry, and recover with a third child.
    #[ntex::test]
    async fn creation_resets_backoff_before_readiness() {
        struct GatedReady {
            id: usize,
            gate: Rc<oneshot::Receiver<()>>,
        }

        impl Service<(), ()> for GatedReady {
            type Res = usize;
            type Error = io::Error;

            /// Suspends and fails the first created child's readiness.
            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                if self.id == 1 {
                    self.gate.recv().await.unwrap();
                    Err(io::Error::other("readiness failed"))
                } else {
                    Ok(())
                }
            }

            /// Identifies the child reached after successful recovery.
            async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<usize, io::Error> {
                Ok(self.id)
            }
        }

        let attempts = Rc::new(Cell::new(0));
        let (release, gate) = oneshot::channel();
        let gate = Rc::new(gate);
        let factory = fn_factory({
            let attempts = attempts.clone();
            move |(): &()| {
                let id = attempts.get();
                attempts.set(id + 1);
                let gate = gate.clone();
                async move {
                    if id == 0 {
                        Err(io::Error::other("creation failed"))
                    } else {
                        Ok(GatedReady { id, gate })
                    }
                }
            }
        });
        let service = Rc::new(ReconnectService::new("creation-reset", factory).retry(2));
        let inner = service.inner.get_ref();
        {
            let mut backoff = inner.backoff.borrow_mut();
            backoff.initial_interval = Duration::from_secs(30);
            backoff.randomization_factor = 0.0;
            backoff.reset();
        }
        let pipeline = Pipeline::new((), service.clone());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        {
            let mut ready = pin!(pipeline.ready());
            assert!(ready.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(attempts.get(), 1);
        assert_eq!(inner.retry.get(), 1);
        assert!(inner.backoff.borrow().current_interval > Duration::from_secs(30));
        let expired_start = {
            let mut backoff = inner.backoff.borrow_mut();
            backoff.start_time -= Duration::from_mins(16);
            backoff.start_time
        };
        inner.retry_at.set(Some(time::now()));
        {
            let mut ready = pin!(pipeline.ready());
            assert!(ready.as_mut().poll(&mut cx).is_pending());
            assert_eq!(attempts.get(), 2);
            assert_eq!(
                inner.backoff.borrow().current_interval,
                Duration::from_secs(30)
            );
            assert!(inner.backoff.borrow().start_time > expired_start);
            assert_eq!(inner.retry.get(), 1);

            release.send(()).unwrap();
            assert!(ready.as_mut().poll(&mut cx).is_pending());
            assert_eq!(attempts.get(), 2);
            assert_eq!(inner.retry.get(), 0);
        }
        inner.retry_at.set(Some(time::now()));
        assert_eq!(pipeline.call(()).await.unwrap(), 2);
        assert_eq!(attempts.get(), 3);
        assert_eq!(inner.retry.get(), 2);
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

            /// Always fails readiness to exercise the exhausted-backoff path.
            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                Err(io::Error::other("readiness failed"))
            }

            /// Completes a request without introducing another failure source.
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
            service
                .inner
                .get_ref()
                .backoff
                .borrow_mut()
                .max_elapsed_time = Some(Duration::ZERO);
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

    /// STATEMENT: Unused and healthy time do not exhaust the next recovery window.
    ///
    /// HOW:
    /// - Age the backoff clock by sixteen minutes before first use or after successful readiness.
    /// - Trigger initial creation, call-error recovery, or readiness-error recovery with one retry.
    /// - Fail creation and verify the configured retry is attempted in each case.
    #[ntex::test]
    async fn idle_time_preserves_retry_window() {
        struct RecoveryProbe {
            fail_ready: Rc<Cell<bool>>,
            changed: ReadyNotifier,
        }

        impl Service<(), ()> for RecoveryProbe {
            type Res = ();
            type Error = io::Error;

            /// Subscribes to the test's readiness failure notification.
            fn bind(&self, ctx: BindCtx<'_>) {
                ctx.subscribe(&self.changed);
            }

            /// Fails readiness after the test invalidates a healthy connection.
            async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), io::Error> {
                if self.fail_ready.get() {
                    Err(io::Error::other("readiness failed"))
                } else {
                    Ok(())
                }
            }

            /// Triggers replacement of a previously healthy connection.
            async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), io::Error> {
                Err(io::Error::other("call failed"))
            }
        }

        for (connected, fail_readiness) in [(false, false), (true, false), (true, true)] {
            let attempts = Rc::new(Cell::new(0));
            let fail_creation = Rc::new(Cell::new(false));
            let fail_ready = Rc::new(Cell::new(false));
            let changed = ReadyNotifier::new();
            let factory = fn_factory({
                let attempts = attempts.clone();
                let fail_creation = fail_creation.clone();
                let fail_ready = fail_ready.clone();
                let changed = changed.clone();
                move |(): &()| {
                    attempts.set(attempts.get() + 1);
                    let fail = fail_creation.get();
                    let fail_ready = fail_ready.clone();
                    let changed = changed.clone();
                    async move {
                        if fail {
                            Err(io::Error::other("creation failed"))
                        } else {
                            Ok(RecoveryProbe {
                                fail_ready,
                                changed,
                            })
                        }
                    }
                }
            });
            let service = Rc::new(if connected {
                ReconnectService::new("idle-window", factory).retry(1)
            } else {
                Reconnect::new("idle-window", factory)
                    .retry(1)
                    .create(&())
                    .await
                    .unwrap()
            });
            let pipeline = Pipeline::new((), service.clone());
            if connected {
                pipeline.ready().await.unwrap();
            }
            {
                let mut backoff = service.inner.get_ref().backoff.borrow_mut();
                backoff.initial_interval = Duration::ZERO;
                backoff.current_interval = Duration::ZERO;
                backoff.start_time -= Duration::from_mins(16);
            }
            if fail_readiness {
                fail_ready.set(true);
                changed.notify();
            } else if connected {
                assert!(pipeline.call(()).await.is_err());
            }
            fail_creation.set(true);
            let before = attempts.get();
            assert_eq!(
                pipeline.ready().await.unwrap_err().to_string(),
                "creation failed"
            );
            assert_eq!(attempts.get() - before, if fail_readiness { 1 } else { 2 });
        }
    }

    /// STATEMENT: Restarting recovery does not renew its elapsed-time window.
    ///
    /// HOW:
    /// - Suspend initial creation or its retry delay, then age the active window past its limit.
    /// - Invalidate and cancel readiness, preserving any outstanding retry deadline.
    /// - Fail the next attempt and a later check, verifying neither gets another retry.
    #[ntex::test]
    async fn retry_window_survives_readiness_restarts() {
        for pending_creation in [false, true] {
            let attempts = Rc::new(Cell::new(0));
            let factory = fn_factory({
                let attempts = attempts.clone();
                move |(): &()| {
                    let attempt = attempts.get();
                    attempts.set(attempt + 1);
                    async move {
                        if pending_creation && attempt == 0 {
                            std::future::pending::<()>().await;
                        }
                        Err::<ReadyBinding<Probe>, _>(io::Error::other("creation failed"))
                    }
                }
            });
            let service = Rc::new(ReconnectService::new("active-window", factory));
            let inner = service.inner.get_ref();
            {
                let mut backoff = inner.backoff.borrow_mut();
                backoff.initial_interval = Duration::from_secs(30);
                backoff.randomization_factor = 0.0;
                backoff.reset();
            }
            let pipeline = Pipeline::new((), service.clone());
            let mut cx = Context::from_waker(std::task::Waker::noop());
            {
                let mut ready = pin!(pipeline.ready());
                assert!(ready.as_mut().poll(&mut cx).is_pending());
                assert_eq!(attempts.get(), 1);
                inner.backoff.borrow_mut().start_time -= Duration::from_mins(16);
                inner.changed.notify();
            }
            if !pending_creation {
                let retry_at = inner.retry_at.get();
                {
                    let mut ready = pin!(pipeline.ready());
                    assert!(ready.as_mut().poll(&mut cx).is_pending());
                }
                assert_eq!(attempts.get(), 1);
                assert_eq!(inner.retry_at.get(), retry_at);
                inner.retry_at.set(Some(time::now()));
            }
            assert_eq!(
                pipeline.ready().await.unwrap_err().to_string(),
                "creation failed"
            );
            assert_eq!(attempts.get(), 2);
            assert!(pipeline.ready().await.is_err());
            assert_eq!(attempts.get(), 3);
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
                    let mut backoff = service.inner.get_ref().backoff.borrow_mut();
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

            /// Marks the reconnect wrapper errored before shutdown is exercised.
            async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), io::Error> {
                Err(io::Error::other("call failed"))
            }

            /// Records shutdown entry and waits for the test to permit completion.
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
