//! Service that applies a timeout to requests.
//!
//! If the response does not complete within the specified timeout, the response
//! will be aborted.
use std::{fmt, marker::PhantomData};

use ntex_service::{Ctx, IntoService, Middleware, Service};

use crate::future::{Either, select};
use crate::time::{Millis, sleep};

/// Applies a timeout to requests.
///
/// Timeout transform is disabled if timeout is set to 0
#[derive(Debug)]
pub struct Timeout<St> {
    timeout: Millis,
    _t: PhantomData<St>,
}

/// Timeout error
pub enum TimeoutError<E> {
    /// Service error
    Service(E),
    /// Service call timeout
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

/// Applies a timeout to requests.
#[derive(Debug, Clone)]
pub struct TimeoutService<S, St> {
    service: S,
    timeout: Millis,
    st: PhantomData<St>,
}

impl<S, St> TimeoutService<S, St> {
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
            match select(sleep(self.timeout), ctx.call(&self.service, req)).await {
                Either::Left(()) => Err(TimeoutError::Timeout),
                Either::Right(res) => res.map_err(TimeoutError::Service),
            }
        }
    }

    ntex_service::forward_ready!(St, service, TimeoutError::Service);
    ntex_service::forward_shutdown!(St, service);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    #[ntex::test]
    async fn test_zero() {
        let wait_time = Duration::from_millis(50);
        let resolution = Duration::from_millis(0);

        let timeout = Pipeline::new((), TimeoutService::new(resolution, SleepService(wait_time)));
        assert_eq!(timeout.call(()).await, Ok(()));
        assert_eq!(timeout.ready().await, Ok(()));
    }

    #[ntex::test]
    async fn test_timeout() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(500);

        let timeout = Pipeline::new((), TimeoutService::new(resolution, SleepService(wait_time)));
        assert_eq!(timeout.call(()).await, Err(TimeoutError::Timeout));
    }

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

    #[test]
    fn test_error() {
        let err1 = TimeoutError::<SrvError>::Timeout;
        assert!(format!("{err1:?}").contains("TimeoutError::Timeout"));
        assert!(format!("{err1}").contains("Service call timeout"));

        let err2: TimeoutError<_> = SrvError.into();
        assert!(format!("{err2:?}").contains("TimeoutError::Service"));
        assert!(format!("{err2}").contains("SrvError"));
    }
}
