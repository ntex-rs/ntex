//! Service that applies a timeout to requests.
//!
//! If the response does not complete within the specified timeout, the response
//! will be aborted.
use std::{
    fmt, future::Future, marker, marker::PhantomData, pin::Pin, task::Context, task::Poll,
};

use ntex_service::{IntoService, Service, Transform};

use crate::future::Either;
use crate::time::{sleep, Millis, Sleep};

/// Applies a timeout to requests.
///
/// Timeout transform is disabled if timeout is set to 0
#[derive(Debug)]
pub struct Timeout<E = ()> {
    timeout: Millis,
    _t: marker::PhantomData<E>,
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
            TimeoutError::Service(e) => write!(f, "TimeoutError::Service({:?})", e),
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

impl Timeout {
    pub fn new<T: Into<Millis>>(timeout: T) -> Self {
        Timeout {
            timeout: timeout.into(),
            _t: marker::PhantomData,
        }
    }
}

impl Clone for Timeout {
    fn clone(&self) -> Self {
        Timeout {
            timeout: self.timeout,
            _t: marker::PhantomData,
        }
    }
}

impl<S> Transform<S> for Timeout {
    type Service = TimeoutService<S>;

    fn new_transform(&self, service: S) -> Self::Service {
        TimeoutService {
            service,
            timeout: self.timeout,
        }
    }
}

/// Applies a timeout to requests.
#[derive(Debug, Clone)]
pub struct TimeoutService<S> {
    service: S,
    timeout: Millis,
}

impl<S> TimeoutService<S> {
    pub fn new<T, U, R>(timeout: T, service: U) -> Self
    where
        T: Into<Millis>,
        S: Service<R>,
        U: IntoService<S, R>,
    {
        TimeoutService {
            timeout: timeout.into(),
            service: service.into_service(),
        }
    }
}

impl<S, R> Service<R> for TimeoutService<S>
where
    S: Service<R>,
{
    type Response = S::Response;
    type Error = TimeoutError<S::Error>;
    type Future = Either<TimeoutServiceResponse<S, R>, TimeoutServiceResponse2<S, R>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(TimeoutError::Service)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, request: R) -> Self::Future {
        if self.timeout.is_zero() {
            Either::Right(TimeoutServiceResponse2 {
                fut: self.service.call(request),
                _t: PhantomData,
            })
        } else {
            Either::Left(TimeoutServiceResponse {
                fut: self.service.call(request),
                sleep: sleep(self.timeout),
                _t: PhantomData,
            })
        }
    }
}

pin_project_lite::pin_project! {
    /// `TimeoutService` response future
    #[doc(hidden)]
    #[derive(Debug)]
    pub struct TimeoutServiceResponse<T: Service<R>, R> {
        #[pin]
        fut: T::Future,
        sleep: Sleep,
        _t: PhantomData<R>
    }
}

impl<T, R> Future for TimeoutServiceResponse<T, R>
where
    T: Service<R>,
{
    type Output = Result<T::Response, TimeoutError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        // First, try polling the future
        match this.fut.poll(cx) {
            Poll::Ready(Ok(v)) => return Poll::Ready(Ok(v)),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(TimeoutError::Service(e))),
            Poll::Pending => {}
        }

        // Now check the sleep
        match this.sleep.poll_elapsed(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(Err(TimeoutError::Timeout)),
        }
    }
}

pin_project_lite::pin_project! {
    /// `TimeoutService` response future
    #[doc(hidden)]
    #[derive(Debug)]
    pub struct TimeoutServiceResponse2<T: Service<R>, R> {
        #[pin]
        fut: T::Future,
        _t: PhantomData<R>,
    }
}

impl<T, R> Future for TimeoutServiceResponse2<T, R>
where
    T: Service<R>,
{
    type Output = Result<T::Response, TimeoutError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project().fut.poll(cx) {
            Poll::Ready(Ok(v)) => Poll::Ready(Ok(v)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(TimeoutError::Service(e))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fmt, task::Context, task::Poll, time::Duration};

    use ntex_service::{apply, fn_factory, Service, ServiceFactory};

    use super::*;
    use crate::future::lazy;

    #[derive(Clone, Debug, PartialEq)]
    struct SleepService(Duration);

    #[derive(Clone, Debug, PartialEq)]
    struct SrvError;

    impl fmt::Display for SrvError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "SrvError")
        }
    }

    impl Service<()> for SleepService {
        type Response = ();
        type Error = SrvError;
        type Future = Pin<Box<dyn Future<Output = Result<(), SrvError>>>>;

        fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(&self, _: &mut Context<'_>, is_error: bool) -> Poll<()> {
            if is_error {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }

        fn call(&self, _: ()) -> Self::Future {
            let fut = crate::time::sleep(self.0);
            Box::pin(async move {
                let _ = fut.await;
                Ok::<_, SrvError>(())
            })
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_success() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(50);

        let timeout = TimeoutService::new(resolution, SleepService(wait_time)).clone();
        assert_eq!(timeout.call(()).await, Ok(()));
        assert!(lazy(|cx| timeout.poll_ready(cx)).await.is_ready());
        assert!(lazy(|cx| timeout.poll_shutdown(cx, true)).await.is_ready());
        assert!(lazy(|cx| timeout.poll_shutdown(cx, false))
            .await
            .is_pending());
    }

    #[ntex_macros::rt_test2]
    async fn test_zero() {
        let wait_time = Duration::from_millis(50);
        let resolution = Duration::from_millis(0);

        let timeout = TimeoutService::new(resolution, SleepService(wait_time));
        assert_eq!(timeout.call(()).await, Ok(()));
        assert!(lazy(|cx| timeout.poll_ready(cx)).await.is_ready());
    }

    #[ntex_macros::rt_test2]
    async fn test_timeout() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(500);

        let timeout = TimeoutService::new(resolution, SleepService(wait_time));
        assert_eq!(timeout.call(()).await, Err(TimeoutError::Timeout));
    }

    #[ntex_macros::rt_test2]
    #[allow(clippy::redundant_clone)]
    async fn test_timeout_newservice() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(500);

        let timeout = apply(
            Timeout::new(resolution).clone(),
            fn_factory(|| async { Ok::<_, ()>(SleepService(wait_time)) }),
        );
        let srv = timeout.new_service(&()).await.unwrap();

        let res = srv.call(()).await.unwrap_err();
        assert_eq!(res, TimeoutError::Timeout);
    }

    #[test]
    fn test_error() {
        let err1 = TimeoutError::<SrvError>::Timeout;
        assert!(format!("{:?}", err1).contains("TimeoutError::Timeout"));
        assert!(format!("{}", err1).contains("Service call timeout"));

        let err2: TimeoutError<_> = SrvError.into();
        assert!(format!("{:?}", err2).contains("TimeoutError::Service"));
        assert!(format!("{}", err2).contains("SrvError"));
    }
}
