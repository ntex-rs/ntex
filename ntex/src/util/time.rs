use std::task::{Context, Poll};
use std::time::{self, Duration, Instant};
use std::{cell::RefCell, convert::Infallible, rc::Rc};

use crate::rt::time::sleep;
use crate::service::{Service, ServiceFactory};
use crate::util::Ready;

#[derive(Clone, Debug)]
pub struct LowResTime(Rc<RefCell<Inner>>);

#[derive(Debug)]
struct Inner {
    resolution: Duration,
    current: Option<Instant>,
}

impl Inner {
    fn new(resolution: Duration) -> Self {
        Inner {
            resolution,
            current: None,
        }
    }
}

impl LowResTime {
    pub fn with(resolution: Duration) -> LowResTime {
        LowResTime(Rc::new(RefCell::new(Inner::new(resolution))))
    }

    pub fn timer(&self) -> LowResTimeService {
        LowResTimeService(self.0.clone())
    }
}

impl Default for LowResTime {
    fn default() -> Self {
        LowResTime(Rc::new(RefCell::new(Inner::new(Duration::from_secs(1)))))
    }
}

impl ServiceFactory for LowResTime {
    type Request = ();
    type Response = Instant;
    type Error = Infallible;
    type InitError = Infallible;
    type Config = ();
    type Service = LowResTimeService;
    type Future = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(self.timer())
    }
}

#[derive(Clone, Debug)]
pub struct LowResTimeService(Rc<RefCell<Inner>>);

impl LowResTimeService {
    pub fn with(resolution: Duration) -> LowResTimeService {
        LowResTimeService(Rc::new(RefCell::new(Inner::new(resolution))))
    }

    /// Get current time. This function has to be called from
    /// future's poll method, otherwise it panics.
    pub fn now(&self) -> Instant {
        let cur = self.0.borrow().current;
        if let Some(cur) = cur {
            cur
        } else {
            let now = Instant::now();
            let inner = self.0.clone();
            let interval = {
                let mut b = inner.borrow_mut();
                b.current = Some(now);
                b.resolution
            };

            crate::rt::spawn(async move {
                sleep(interval).await;
                inner.borrow_mut().current.take();
            });
            now
        }
    }
}

impl Service for LowResTimeService {
    type Request = ();
    type Response = Instant;
    type Error = Infallible;
    type Future = Ready<Self::Response, Self::Error>;

    #[inline]
    fn poll_ready(&self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&self, _: ()) -> Self::Future {
        Ready::Ok(self.now())
    }
}

#[derive(Clone, Debug)]
pub struct SystemTime(Rc<RefCell<SystemTimeInner>>);

#[derive(Debug)]
struct SystemTimeInner {
    resolution: Duration,
    current: Option<time::SystemTime>,
}

impl SystemTimeInner {
    fn new(resolution: Duration) -> Self {
        SystemTimeInner {
            resolution,
            current: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SystemTimeService(Rc<RefCell<SystemTimeInner>>);

impl SystemTimeService {
    pub fn with(resolution: Duration) -> SystemTimeService {
        SystemTimeService(Rc::new(RefCell::new(SystemTimeInner::new(resolution))))
    }

    /// Get current time. This function has to be called from
    /// future's poll method, otherwise it panics.
    pub fn now(&self) -> time::SystemTime {
        let cur = self.0.borrow().current;
        if let Some(cur) = cur {
            cur
        } else {
            let now = time::SystemTime::now();
            let inner = self.0.clone();
            let interval = {
                let mut b = inner.borrow_mut();
                b.current = Some(now);
                b.resolution
            };

            crate::rt::spawn(async move {
                sleep(interval).await;
                inner.borrow_mut().current.take();
            });
            now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::lazy;
    use std::time::{Duration, SystemTime};

    #[crate::rt_test]
    async fn low_res_timee() {
        let f = LowResTime::default();
        let srv = f.new_service(()).await.unwrap();
        assert!(lazy(|cx| srv.poll_ready(cx)).await.is_ready());
        srv.call(()).await.unwrap();
    }

    /// State Under Test: Two calls of `SystemTimeService::now()` return the same value if they are done within resolution interval of `SystemTimeService`.
    ///
    /// Expected Behavior: Two back-to-back calls of `SystemTimeService::now()` return the same value.
    #[crate::rt_test]
    async fn system_time_service_time_does_not_immediately_change() {
        let resolution = Duration::from_millis(50);

        let time_service = SystemTimeService::with(resolution);
        assert_eq!(time_service.now(), time_service.now());
    }

    /// State Under Test: Two calls of `LowResTimeService::now()` return the same value if they are done within resolution interval of `SystemTimeService`.
    ///
    /// Expected Behavior: Two back-to-back calls of `LowResTimeService::now()` return the same value.
    #[crate::rt_test]
    async fn lowres_time_service_time_does_not_immediately_change() {
        let resolution = Duration::from_millis(50);
        let time_service = LowResTimeService::with(resolution);
        assert_eq!(time_service.now(), time_service.now());
    }

    /// State Under Test: `SystemTimeService::now()` updates returned value every resolution period.
    ///
    /// Expected Behavior: Two calls of `LowResTimeService::now()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a resolution interval.
    #[crate::rt_test]
    async fn system_time_service_time_updates_after_resolution_interval() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(300);

        let time_service = SystemTimeService::with(resolution);

        let first_time = time_service
            .now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();

        sleep(wait_time).await;

        let second_time = time_service
            .now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();

        assert!(second_time - first_time >= wait_time);
    }

    /// State Under Test: `LowResTimeService::now()` updates returned value every resolution period.
    ///
    /// Expected Behavior: Two calls of `LowResTimeService::now()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a resolution interval.
    #[crate::rt_test]
    async fn lowres_time_service_time_updates_after_resolution_interval() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(300);
        let time_service = LowResTimeService::with(resolution);

        let first_time = time_service.now();

        sleep(wait_time).await;

        let second_time = time_service.now();
        assert!(second_time - first_time >= wait_time);
    }
}
