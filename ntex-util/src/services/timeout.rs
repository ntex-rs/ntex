//! Service that applies a timeout to request execution.
//!
//! If a service call does not complete within the configured timeout, its
//! future is dropped and [`TimeoutError::Timeout`] is returned.
//! If both complete during the same poll, the service result wins.
//!
//! The timeout covers execution only. The clock starts when the wrapped service
//! is called, after the pipeline has waited for readiness. Waiting for a busy or
//! never-ready service is not bounded by this timeout, because readiness is a
//! gate shared by all callers rather than part of one request. To bound the
//! whole operation, including the readiness wait, wrap the pipeline call:
//!
//! ```rust
//! use ntex_service::Pipeline;
//! use ntex_util::time::{Millis, timeout};
//!
//! async fn call_with_deadline(
//!     pl: &Pipeline<u32, u32, std::io::Error>,
//! ) -> Option<Result<u32, std::io::Error>> {
//!     // `Err(())` from `timeout` means the deadline elapsed
//!     timeout(Millis(500), pl.call(1)).await.ok()
//! }
//! ```
use std::{fmt, marker::PhantomData};

use ntex_service::{Ctx, IntoService, Middleware, Service};

use crate::time::{Millis, timeout};

/// Applies a timeout to the execution of each request.
///
/// The timeout starts when the wrapped service is called and does not include
/// the wait for readiness; see the [module docs](self) for bounding the whole
/// operation. A zero timeout disables the middleware.
#[derive(Debug)]
pub struct Timeout<St> {
    timeout: Millis,
    _t: PhantomData<St>,
}

/// Error returned by a timed service call.
pub enum TimeoutError<E> {
    /// Error returned by the wrapped service.
    Service(E),
    /// The service call exceeded its timeout.
    ///
    /// Time spent waiting for readiness before the call does not count.
    Timeout,
}

impl<E> From<E> for TimeoutError<E> {
    fn from(err: E) -> Self {
        TimeoutError::Service(err)
    }
}

impl<E: fmt::Debug> fmt::Debug for TimeoutError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeoutError::Service(e) => write!(f, "TimeoutError::Service({e:?})"),
            TimeoutError::Timeout => write!(f, "TimeoutError::Timeout"),
        }
    }
}

impl<E: fmt::Display> fmt::Display for TimeoutError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeoutError::Service(e) => e.fmt(f),
            TimeoutError::Timeout => write!(f, "Service call timeout"),
        }
    }
}

impl<E: fmt::Display + fmt::Debug> std::error::Error for TimeoutError<E> {}

impl<E: PartialEq> PartialEq for TimeoutError<E> {
    fn eq(&self, other: &TimeoutError<E>) -> bool {
        match self {
            TimeoutError::Service(e1) => match other {
                TimeoutError::Service(e2) => e1 == e2,
                TimeoutError::Timeout => false,
            },
            TimeoutError::Timeout => match other {
                TimeoutError::Service(_) => false,
                TimeoutError::Timeout => true,
            },
        }
    }
}

impl<St> Timeout<St> {
    /// Creates timeout middleware with the specified duration.
    pub fn new<T: Into<Millis>>(timeout: T) -> Self {
        Timeout {
            timeout: timeout.into(),
            _t: PhantomData,
        }
    }
}

impl<St> Clone for Timeout<St> {
    fn clone(&self) -> Self {
        Timeout {
            timeout: self.timeout,
            _t: PhantomData,
        }
    }
}

impl<S, St> Middleware<S, St> for Timeout<St> {
    type Service = TimeoutService<S, St>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        TimeoutService {
            service,
            timeout: self.timeout,
            st: PhantomData,
        }
    }
}

/// A service that applies a timeout to the execution of each request.
///
/// Readiness is forwarded to the wrapped service without a deadline, so a
/// request waiting for readiness is not timed out; see the
/// [module docs](self) for bounding the whole operation.
#[derive(Debug, Clone)]
pub struct TimeoutService<S, St> {
    service: S,
    timeout: Millis,
    st: PhantomData<St>,
}

impl<S, St> TimeoutService<S, St> {
    /// Wraps a service with the specified per-request execution timeout.
    pub fn new<T, Req>(timeout: T, service: impl IntoService<S, St, Req>) -> Self
    where
        T: Into<Millis>,
        S: Service<St, Req>,
    {
        TimeoutService {
            service: service.into_service(),
            timeout: timeout.into(),
            st: PhantomData,
        }
    }
}

impl<S, St, Req> Service<St, Req> for TimeoutService<S, St>
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = TimeoutError<S::Error>;

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<S::Res, Self::Error> {
        if self.timeout.is_zero() {
            ctx.call(&self.service, req)
                .await
                .map_err(TimeoutError::Service)
        } else {
            match timeout(self.timeout, ctx.call(&self.service, req)).await {
                Ok(res) => res.map_err(TimeoutError::Service),
                Err(()) => Err(TimeoutError::Timeout),
            }
        }
    }

    ntex_service::forward!(St, service, map_err = TimeoutError::Service);
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, future::pending, rc::Rc, time::Duration};

    use ntex_service::{Pipeline, apply, fn_factory};

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct SleepService(Duration);

    #[derive(Clone, Debug, PartialEq)]
    struct SrvError;

    impl fmt::Display for SrvError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "SrvError")
        }
    }

    impl Service<(), ()> for SleepService {
        type Res = ();
        type Error = SrvError;

        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), SrvError> {
            crate::time::sleep(self.0).await;
            Ok::<_, SrvError>(())
        }
    }

    /// STATEMENT: A service finishing before its timeout returns success.
    ///
    /// HOW:
    /// - Clone a timeout service whose inner delay is shorter than its deadline.
    /// - Verify a successful call and readiness, then complete shutdown.
    #[ntex::test]
    async fn test_success() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(50);

        let timeout = Pipeline::new(
            (),
            TimeoutService::new(resolution, SleepService(wait_time)).clone(),
        );
        assert_eq!(timeout.call(()).await, Ok(()));
        assert_eq!(timeout.ready().await, Ok(()));
        timeout.shutdown().await;
    }

    /// STATEMENT: A zero timeout disables the deadline rather than rejecting delayed calls.
    ///
    /// HOW:
    /// - Wrap a sleeping service with a zero timeout and verify its call and readiness succeed.
    #[ntex::test]
    async fn test_zero() {
        let wait_time = Duration::from_millis(50);
        let resolution = Duration::from_millis(0);

        let timeout = Pipeline::new((), TimeoutService::new(resolution, SleepService(wait_time)));
        assert_eq!(timeout.call(()).await, Ok(()));
        assert_eq!(timeout.ready().await, Ok(()));
    }

    /// STATEMENT: Calls of a ready service exceeding their deadline return `TimeoutError::Timeout`.
    ///
    /// HOW:
    /// - Give an always-ready sleeping service a shorter timeout and compare the returned error.
    #[ntex::test]
    async fn test_timeout() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(500);

        let timeout = Pipeline::new((), TimeoutService::new(resolution, SleepService(wait_time)));
        assert_eq!(timeout.call(()).await, Err(TimeoutError::Timeout));
    }

    struct GatedService {
        ready_delay: Option<Duration>,
        calls: Rc<Cell<usize>>,
    }

    impl Service<(), ()> for GatedService {
        type Res = ();
        type Error = SrvError;

        async fn ready(&self, _: Ctx<'_, Self>) -> Result<(), SrvError> {
            match self.ready_delay {
                Some(delay) => crate::time::sleep(delay).await,
                None => pending().await,
            }
            Ok(())
        }

        async fn call(&self, (): (), _: Ctx<'_, Self>) -> Result<(), SrvError> {
            self.calls.set(self.calls.get() + 1);
            Ok(())
        }
    }

    /// STATEMENT: The execution timeout does not include the wait for readiness.
    ///
    /// HOW:
    /// - Wrap a service that becomes ready after 100 ms and answers immediately with a 10 ms timeout.
    /// - Verify the pipeline call succeeds instead of returning `TimeoutError::Timeout`.
    /// - Check the wrapped service was called once and at least 100 ms elapsed.
    #[ntex::test]
    async fn test_timeout_excludes_readiness() {
        let calls = Rc::new(Cell::new(0));
        let srv = GatedService {
            ready_delay: Some(Duration::from_millis(100)),
            calls: calls.clone(),
        };
        let pl = Pipeline::new((), TimeoutService::new(Millis(10), srv));

        let start = std::time::Instant::now();
        assert_eq!(pl.call(()).await, Ok(()));
        assert_eq!(calls.get(), 1);
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    /// STATEMENT: An outer `time::timeout` bounds a pipeline call whose service never becomes ready.
    ///
    /// HOW:
    /// - Wrap a never-ready service with a 1 ms execution timeout.
    /// - Bound `Pipeline::call` with a 100 ms `time::timeout` and verify it elapses with `Err(())`.
    /// - Check the wrapped service was never called.
    #[ntex::test]
    async fn test_outer_timeout_bounds_readiness() {
        let calls = Rc::new(Cell::new(0));
        let srv = GatedService {
            ready_delay: None,
            calls: calls.clone(),
        };
        let pl = Pipeline::new((), TimeoutService::new(Millis(1), srv));

        assert_eq!(timeout(Millis(100), pl.call(())).await, Err(()));
        assert_eq!(calls.get(), 0);
    }

    /// STATEMENT: Factory-applied timeout middleware enforces its configured deadline.
    ///
    /// HOW:
    /// - Apply cloned timeout middleware to a factory producing a slower service.
    /// - Create its pipeline and verify the call returns the timeout variant.
    #[ntex::test]
    #[allow(clippy::redundant_clone)]
    async fn test_timeout_middleware() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(500);

        let timeout = apply(
            Timeout::new(resolution).clone(),
            fn_factory(async move |()| Ok::<_, ()>(SleepService(wait_time))),
        );
        let srv = timeout.pipeline(()).await.unwrap();

        let res = srv.call(()).await.unwrap_err();
        assert_eq!(res, TimeoutError::Timeout);
    }

    /// STATEMENT: Timeout errors distinguish elapsed deadlines from wrapped service errors.
    ///
    /// HOW:
    /// - Format a timeout variant and a converted service error.
    /// - Check their debug variants and human-readable messages.
    #[test]
    fn test_error() {
        let err1 = TimeoutError::<SrvError>::Timeout;
        assert!(format!("{err1:?}").contains("TimeoutError::Timeout"));
        assert!(format!("{err1}").contains("Service call timeout"));

        let err2: TimeoutError<_> = SrvError.into();
        assert!(format!("{err2:?}").contains("TimeoutError::Service"));
        assert!(format!("{err2}").contains("SrvError"));
    }

    /// STATEMENT: Timeout-error equality compares both the variant and any service error.
    ///
    /// HOW:
    /// - Compare matching and differing service errors, mixed variants, and two timeout variants.
    #[test]
    fn timeout_error_eq() {
        assert_eq!(TimeoutError::Service(1), TimeoutError::Service(1));
        assert_ne!(TimeoutError::Service(1), TimeoutError::Service(2));
        assert_ne!(TimeoutError::Service(1), TimeoutError::Timeout);
        assert_ne!(TimeoutError::Timeout, TimeoutError::Service(1));
        assert_eq!(TimeoutError::<u8>::Timeout, TimeoutError::Timeout);
    }
}
