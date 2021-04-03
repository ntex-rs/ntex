//! Service that applies a timeout to requests.
//!
//! If the response does not complete within the specified timeout, the response
//! will be aborted.
use std::{fmt, future::Future, marker, pin::Pin, task::Context, task::Poll, time};

use crate::rt::time::{sleep, Sleep};
use crate::service::{IntoService, Service, Transform};
use crate::util::{Either, Ready};

const ZERO: time::Duration = time::Duration::from_millis(0);

/// Applies a timeout to requests.
///
/// Timeout transform is disabled if timeout is set to 0
#[derive(Debug)]
pub struct Timeout<E = ()> {
    timeout: time::Duration,
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

impl<E> Timeout<E> {
    pub fn new(timeout: time::Duration) -> Self {
        Timeout {
            timeout,
            _t: marker::PhantomData,
        }
    }
}

impl<E> Clone for Timeout<E> {
    fn clone(&self) -> Self {
        Timeout::new(self.timeout)
    }
}

impl<S, E> Transform<S> for Timeout<E>
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = TimeoutError<S::Error>;
    type InitError = E;
    type Transform = TimeoutService<S>;
    type Future = Ready<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        Ready::Ok(TimeoutService {
            service,
            timeout: self.timeout,
        })
    }
}

/// Applies a timeout to requests.
#[derive(Debug, Clone)]
pub struct TimeoutService<S> {
    service: S,
    timeout: time::Duration,
}

impl<S> TimeoutService<S>
where
    S: Service,
{
    pub fn new<U>(timeout: time::Duration, service: U) -> Self
    where
        U: IntoService<S>,
    {
        TimeoutService {
            timeout,
            service: service.into_service(),
        }
    }
}

impl<S> Service for TimeoutService<S>
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = TimeoutError<S::Error>;
    type Future = Either<TimeoutServiceResponse<S>, TimeoutServiceResponse2<S>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(TimeoutError::Service)
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.service.poll_shutdown(cx, is_error)
    }

    fn call(&self, request: S::Request) -> Self::Future {
        if self.timeout == ZERO {
            Either::Right(TimeoutServiceResponse2 {
                fut: self.service.call(request),
            })
        } else {
            Either::Left(TimeoutServiceResponse {
                fut: self.service.call(request),
                sleep: Box::pin(sleep(self.timeout)),
            })
        }
    }
}

pin_project_lite::pin_project! {
    /// `TimeoutService` response future
    #[doc(hidden)]
    #[derive(Debug)]
    pub struct TimeoutServiceResponse<T: Service> {
        #[pin]
        fut: T::Future,
        sleep: Pin<Box<Sleep>>,
    }
}

impl<T> Future for TimeoutServiceResponse<T>
where
    T: Service,
{
    type Output = Result<T::Response, TimeoutError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        // First, try polling the future
        match this.fut.poll(cx) {
            Poll::Ready(Ok(v)) => return Poll::Ready(Ok(v)),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(TimeoutError::Service(e))),
            Poll::Pending => {}
        }

        // Now check the sleep
        match Pin::new(&mut this.sleep).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(Err(TimeoutError::Timeout)),
        }
    }
}

pin_project_lite::pin_project! {
    /// `TimeoutService` response future
    #[doc(hidden)]
    #[derive(Debug)]
    pub struct TimeoutServiceResponse2<T: Service> {
        #[pin]
        fut: T::Future,
    }
}

impl<T> Future for TimeoutServiceResponse2<T>
where
    T: Service,
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
    use derive_more::Display;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;
    use crate::service::{apply, fn_factory, Service, ServiceFactory};
    use crate::util::lazy;

    #[derive(Clone, Debug, PartialEq)]
    struct SleepService(Duration);

    #[derive(Clone, Debug, Display, PartialEq)]
    struct SrvError;

    impl Service for SleepService {
        type Request = ();
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
            let fut = crate::rt::time::sleep(self.0);
            Box::pin(async move {
                let _ = fut.await;
                Ok::<_, SrvError>(())
            })
        }
    }

    #[crate::rt_test]
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

    #[crate::rt_test]
    async fn test_zero() {
        let wait_time = Duration::from_millis(50);
        let resolution = Duration::from_millis(0);

        let timeout = TimeoutService::new(resolution, SleepService(wait_time));
        assert_eq!(timeout.call(()).await, Ok(()));
        assert!(lazy(|cx| timeout.poll_ready(cx)).await.is_ready());
    }

    #[crate::rt_test]
    async fn test_timeout() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(500);

        let timeout = TimeoutService::new(resolution, SleepService(wait_time));
        assert_eq!(timeout.call(()).await, Err(TimeoutError::Timeout));
    }

    #[crate::rt_test]
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
