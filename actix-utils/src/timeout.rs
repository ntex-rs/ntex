//! Service that applies a timeout to requests.
//!
//! If the response does not complete within the specified timeout, the response
//! will be aborted.
use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

use actix_service::{NewTransform, Service, Transform};
use futures::future::{ok, FutureResult};
use futures::{Async, Future, Poll};
use tokio_timer::{clock, Delay};

/// Applies a timeout to requests.
#[derive(Debug)]
pub struct Timeout<E = ()> {
    timeout: Duration,
    _t: PhantomData<E>,
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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TimeoutError::Service(e) => write!(f, "TimeoutError::Service({:?})", e),
            TimeoutError::Timeout => write!(f, "TimeoutError::Timeout"),
        }
    }
}

impl<E: fmt::Display> fmt::Display for TimeoutError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
    pub fn new(timeout: Duration) -> Self {
        Timeout {
            timeout,
            _t: PhantomData,
        }
    }
}

impl<E> Clone for Timeout<E> {
    fn clone(&self) -> Self {
        Timeout::new(self.timeout)
    }
}

impl<S, E> NewTransform<S> for Timeout<E>
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = TimeoutError<S::Error>;
    type InitError = E;
    type Transform = TimeoutService;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self) -> Self::Future {
        ok(TimeoutService {
            timeout: self.timeout,
        })
    }
}

/// Applies a timeout to requests.
#[derive(Debug, Clone)]
pub struct TimeoutService {
    timeout: Duration,
}

impl TimeoutService {
    pub fn new(timeout: Duration) -> Self {
        TimeoutService { timeout }
    }
}

impl<S> Transform<S> for TimeoutService
where
    S: Service,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = TimeoutError<S::Error>;
    type Future = TimeoutServiceResponse<S>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, request: S::Request, service: &mut S) -> Self::Future {
        TimeoutServiceResponse {
            fut: service.call(request),
            sleep: Delay::new(clock::now() + self.timeout),
        }
    }
}

/// `TimeoutService` response future
#[derive(Debug)]
pub struct TimeoutServiceResponse<T: Service> {
    fut: T::Future,
    sleep: Delay,
}

impl<T> Future for TimeoutServiceResponse<T>
where
    T: Service,
{
    type Item = T::Response;
    type Error = TimeoutError<T::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // First, try polling the future
        match self.fut.poll() {
            Ok(Async::Ready(v)) => return Ok(Async::Ready(v)),
            Ok(Async::NotReady) => {}
            Err(e) => return Err(TimeoutError::Service(e)),
        }

        // Now check the sleep
        match self.sleep.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(_)) => Err(TimeoutError::Timeout),
            Err(_) => Err(TimeoutError::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::future::lazy;
    use futures::{Async, Poll};

    use std::time::Duration;

    use super::*;
    use actix_service::{Blank, BlankNewService, NewService, Service, ServiceExt};

    struct SleepService(Duration);

    impl Service for SleepService {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = Box<Future<Item = (), Error = ()>>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(Async::Ready(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            Box::new(tokio_timer::sleep(self.0).map_err(|_| ()))
        }
    }

    #[test]
    fn test_success() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(50);

        let res = actix_rt::System::new("test").block_on(lazy(|| {
            let mut timeout = Blank::default()
                .apply(TimeoutService::new(resolution), SleepService(wait_time));
            timeout.call(())
        }));
        assert_eq!(res, Ok(()));
    }

    #[test]
    fn test_timeout() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(150);

        let res = actix_rt::System::new("test").block_on(lazy(|| {
            let mut timeout = Blank::default()
                .apply(TimeoutService::new(resolution), SleepService(wait_time));
            timeout.call(())
        }));
        assert_eq!(res, Err(TimeoutError::Timeout));
    }

    #[test]
    fn test_timeout_newservice() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(150);

        let res = actix_rt::System::new("test").block_on(lazy(|| {
            let timeout = BlankNewService::<_, _, ()>::default()
                .apply(Timeout::new(resolution), || Ok(SleepService(wait_time)));
            if let Async::Ready(mut to) = timeout.new_service().poll().unwrap() {
                to.call(())
            } else {
                panic!()
            }
        }));
        assert_eq!(res, Err(TimeoutError::Timeout));
    }
}
