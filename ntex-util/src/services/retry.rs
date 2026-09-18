use ntex_service::{Ctx, Middleware, Service};

/// Determines whether and how a failed service call is retried.
pub trait Policy<S: Service<St, Req>, St, Req>: Sized + Clone {
    /// Returns whether the call should be retried.
    async fn retry(&mut self, req: &Req, res: &Result<S::Res, S::Error>) -> bool;

    /// Clones or reconstructs a request for a possible retry.
    ///
    /// Returning `None` prevents retries for this request.
    fn clone_request(&self, req: &Req) -> Option<Req>;
}

#[derive(Clone, Debug)]
/// Middleware that retries service calls according to a [`Policy`].
///
/// The policy is cloned for each request.
pub struct Retry<P> {
    policy: P,
}

#[derive(Clone, Debug)]
/// A service that retries calls according to a [`Policy`].
pub struct RetryService<P, S> {
    policy: P,
    service: S,
}

impl<P> Retry<P> {
    /// Creates retry middleware with the specified policy.
    pub fn new(policy: P) -> Self {
        Retry { policy }
    }
}

impl<P: Clone, S, St> Middleware<S, St> for Retry<P> {
    type Service = RetryService<P, S>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        RetryService {
            service,
            policy: self.policy.clone(),
        }
    }
}

impl<P, S> RetryService<P, S> {
    /// Wraps a service with the specified retry policy.
    pub fn new(policy: P, service: S) -> Self {
        RetryService { policy, service }
    }
}

impl<P, S, St, Req> Service<St, Req> for RetryService<P, S>
where
    P: Policy<S, St, Req>,
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    async fn call(&self, mut req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, S::Error> {
        let mut policy = self.policy.clone();
        let mut cloned = policy.clone_request(&req);

        loop {
            let result = ctx.call(&self.service, req).await;

            cloned = if let Some(r) = cloned.take() {
                if policy.retry(&r, &result).await {
                    req = r;
                    policy.clone_request(&req)
                } else {
                    return result;
                }
            } else {
                return result;
            }
        }
    }

    ntex_service::forward_ready!(St, service);
    ntex_service::forward_shutdown!(St, service);
}

#[derive(Copy, Clone, Debug)]
/// A retry policy that retries every service error.
///
/// The default policy permits up to three retries after the initial call.
pub struct DefaultRetryPolicy(u16);

impl DefaultRetryPolicy {
    /// Creates a policy that permits up to `retry` retries.
    pub fn new(retry: u16) -> Self {
        DefaultRetryPolicy(retry)
    }
}

impl Default for DefaultRetryPolicy {
    fn default() -> Self {
        DefaultRetryPolicy::new(3)
    }
}

impl<S, St, Req> Policy<S, St, Req> for DefaultRetryPolicy
where
    S: Service<St, Req>,
    Req: Clone,
{
    async fn retry(&mut self, _: &Req, res: &Result<S::Res, S::Error>) -> bool {
        if res.is_err() {
            if self.0 == 0 {
                false
            } else {
                self.0 -= 1;
                true
            }
        } else {
            false
        }
    }

    fn clone_request(&self, req: &Req) -> Option<Req> {
        Some(req.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use std::{cell::Cell, rc::Rc};

    use ntex_service::{Pipeline, apply, fn_factory};

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct TestService(Rc<Cell<usize>>);

    impl Service<(), ()> for TestService {
        type Res = ();
        type Error = ();

        async fn call(&self, _r: (), _: Ctx<'_, Self>) -> Result<(), ()> {
            let cnt = self.0.get();
            if cnt == 0 {
                Ok(())
            } else {
                self.0.set(cnt - 1);
                Err(())
            }
        }
    }

    #[ntex::test]
    async fn test_retry() {
        let cnt = Rc::new(Cell::new(5));
        let svc = Pipeline::new(
            (),
            RetryService::new(DefaultRetryPolicy::default(), TestService(cnt.clone())).clone(),
        );
        assert_eq!(svc.call(()).await, Err(()));
        assert_eq!(svc.ready().await, Ok(()));
        svc.shutdown().await;
        assert_eq!(cnt.get(), 1);

        let factory = apply(
            Retry::new(DefaultRetryPolicy::new(3)).clone(),
            fn_factory(|(): &()| async { Ok::<_, ()>(TestService(Rc::new(Cell::new(2)))) }),
        );
        let srv = factory.pipeline(()).await.unwrap();
        assert_eq!(srv.call(()).await, Ok(()));

        let factory = apply(
            Retry::new(DefaultRetryPolicy::new(3)).clone(),
            fn_factory(|(): &()| async { Ok::<_, ()>(TestService(Rc::new(Cell::new(2)))) }),
        );
        let srv = factory.pipeline(()).await.unwrap();
        assert_eq!(srv.call(()).await, Ok(()));
    }
}
