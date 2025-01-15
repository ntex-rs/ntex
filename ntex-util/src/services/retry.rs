#![allow(async_fn_in_trait)]
use ntex_service::{Middleware, Service, ServiceCtx};

/// Trait defines retry policy
pub trait Policy<Req, S: Service<Req>>: Sized + Clone {
    async fn retry(&mut self, req: &Req, res: &Result<S::Response, S::Error>) -> bool;

    fn clone_request(&self, req: &Req) -> Option<Req>;
}

#[derive(Clone, Debug)]
/// Retry middleware
///
/// Retry middleware allows to retry service call
pub struct Retry<P> {
    policy: P,
}

#[derive(Clone, Debug)]
/// Retry service
///
/// Retry service allows to retry service call
pub struct RetryService<P, S> {
    policy: P,
    service: S,
}

impl<P> Retry<P> {
    /// Create retry middleware
    pub fn new(policy: P) -> Self {
        Retry { policy }
    }
}

impl<P: Clone, S> Middleware<S> for Retry<P> {
    type Service = RetryService<P, S>;

    fn create(&self, service: S) -> Self::Service {
        RetryService {
            service,
            policy: self.policy.clone(),
        }
    }
}

impl<P, S> RetryService<P, S> {
    /// Create retry service
    pub fn new(policy: P, service: S) -> Self {
        RetryService { policy, service }
    }
}

impl<P, S, R> Service<R> for RetryService<P, S>
where
    P: Policy<R, S>,
    S: Service<R>,
{
    type Response = S::Response;
    type Error = S::Error;

    ntex_service::forward_poll!(service);
    ntex_service::forward_ready!(service);
    ntex_service::forward_shutdown!(service);

    async fn call(
        &self,
        mut request: R,
        ctx: ServiceCtx<'_, Self>,
    ) -> Result<S::Response, S::Error> {
        let mut policy = self.policy.clone();
        let mut cloned = policy.clone_request(&request);

        loop {
            let result = ctx.call(&self.service, request).await;

            cloned = if let Some(req) = cloned.take() {
                if policy.retry(&req, &result).await {
                    request = req;
                    policy.clone_request(&request)
                } else {
                    return result;
                }
            } else {
                return result;
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
/// Default retry policy
///
/// This policy retries on any error. By default retry count is 3
pub struct DefaultRetryPolicy(u16);

impl DefaultRetryPolicy {
    /// Create default retry policy
    pub fn new(retry: u16) -> Self {
        DefaultRetryPolicy(retry)
    }
}

impl Default for DefaultRetryPolicy {
    fn default() -> Self {
        DefaultRetryPolicy::new(3)
    }
}

impl<R, S> Policy<R, S> for DefaultRetryPolicy
where
    R: Clone,
    S: Service<R>,
{
    async fn retry(&mut self, _: &R, res: &Result<S::Response, S::Error>) -> bool {
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

    fn clone_request(&self, req: &R) -> Option<R> {
        Some(req.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use ntex_service::{apply, fn_factory, Pipeline, ServiceFactory};

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct TestService(Rc<Cell<usize>>);

    impl Service<()> for TestService {
        type Response = ();
        type Error = ();

        async fn call(&self, _: (), _: ServiceCtx<'_, Self>) -> Result<(), ()> {
            let cnt = self.0.get();
            if cnt == 0 {
                Ok(())
            } else {
                self.0.set(cnt - 1);
                Err(())
            }
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_retry() {
        let cnt = Rc::new(Cell::new(5));
        let svc = Pipeline::new(
            RetryService::new(DefaultRetryPolicy::default(), TestService(cnt.clone()))
                .clone(),
        );
        assert_eq!(svc.call(()).await, Err(()));
        assert_eq!(svc.ready().await, Ok(()));
        svc.shutdown().await;
        assert_eq!(cnt.get(), 1);

        let factory = apply(
            Retry::new(DefaultRetryPolicy::new(3)).clone(),
            fn_factory(|| async { Ok::<_, ()>(TestService(Rc::new(Cell::new(2)))) }),
        );
        let srv = factory.pipeline(&()).await.unwrap();
        assert_eq!(srv.call(()).await, Ok(()));
    }
}
