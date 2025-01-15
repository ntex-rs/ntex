#![allow(async_fn_in_trait)]
use ntex_service::{Service, ServiceCtx, Middleware};

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
