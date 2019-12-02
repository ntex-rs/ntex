use std::convert::Infallible;
use std::task::{Context, Poll};
use std::time::{self, Duration, Instant};

use actix_rt::time::delay_for;
use actix_service::{Service, ServiceFactory};
use futures::future::{ok, ready, FutureExt, Ready};

use super::cell::Cell;

#[derive(Clone, Debug)]
pub struct LowResTime(Cell<Inner>);

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
        LowResTime(Cell::new(Inner::new(resolution)))
    }

    pub fn timer(&self) -> LowResTimeService {
        LowResTimeService(self.0.clone())
    }
}

impl Default for LowResTime {
    fn default() -> Self {
        LowResTime(Cell::new(Inner::new(Duration::from_secs(1))))
    }
}

impl ServiceFactory for LowResTime {
    type Request = ();
    type Response = Instant;
    type Error = Infallible;
    type InitError = Infallible;
    type Config = ();
    type Service = LowResTimeService;
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(self.timer())
    }
}

#[derive(Clone, Debug)]
pub struct LowResTimeService(Cell<Inner>);

impl LowResTimeService {
    pub fn with(resolution: Duration) -> LowResTimeService {
        LowResTimeService(Cell::new(Inner::new(resolution)))
    }

    /// Get current time. This function has to be called from
    /// future's poll method, otherwise it panics.
    pub fn now(&self) -> Instant {
        let cur = self.0.get_ref().current;
        if let Some(cur) = cur {
            cur
        } else {
            let now = Instant::now();
            let mut inner = self.0.clone();
            let interval = {
                let mut b = inner.get_mut();
                b.current = Some(now);
                b.resolution
            };

            actix_rt::spawn(delay_for(interval).then(move |_| {
                inner.get_mut().current.take();
                ready(())
            }));
            now
        }
    }
}

impl Service for LowResTimeService {
    type Request = ();
    type Response = Instant;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: ()) -> Self::Future {
        ok(self.now())
    }
}

#[derive(Clone, Debug)]
pub struct SystemTime(Cell<SystemTimeInner>);

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
pub struct SystemTimeService(Cell<SystemTimeInner>);

impl SystemTimeService {
    pub fn with(resolution: Duration) -> SystemTimeService {
        SystemTimeService(Cell::new(SystemTimeInner::new(resolution)))
    }

    /// Get current time. This function has to be called from
    /// future's poll method, otherwise it panics.
    pub fn now(&self) -> time::SystemTime {
        let cur = self.0.get_ref().current;
        if let Some(cur) = cur {
            cur
        } else {
            let now = time::SystemTime::now();
            let mut inner = self.0.clone();
            let interval = {
                let mut b = inner.get_mut();
                b.current = Some(now);
                b.resolution
            };

            actix_rt::spawn(delay_for(interval).then(move |_| {
                inner.get_mut().current.take();
                ready(())
            }));
            now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// State Under Test: Two calls of `SystemTimeService::now()` return the same value if they are done within resolution interval of `SystemTimeService`.
    ///
    /// Expected Behavior: Two back-to-back calls of `SystemTimeService::now()` return the same value.
    #[actix_rt::test]
    async fn system_time_service_time_does_not_immediately_change() {
        let resolution = Duration::from_millis(50);

        let time_service = SystemTimeService::with(resolution);
        assert_eq!(time_service.now(), time_service.now());
    }

    /// State Under Test: Two calls of `LowResTimeService::now()` return the same value if they are done within resolution interval of `SystemTimeService`.
    ///
    /// Expected Behavior: Two back-to-back calls of `LowResTimeService::now()` return the same value.
    #[actix_rt::test]
    async fn lowres_time_service_time_does_not_immediately_change() {
        let resolution = Duration::from_millis(50);
        let time_service = LowResTimeService::with(resolution);
        assert_eq!(time_service.now(), time_service.now());
    }

    /// State Under Test: `SystemTimeService::now()` updates returned value every resolution period.
    ///
    /// Expected Behavior: Two calls of `LowResTimeService::now()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a resolution interval.
    #[actix_rt::test]
    async fn system_time_service_time_updates_after_resolution_interval() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(150);

        let time_service = SystemTimeService::with(resolution);

        let first_time = time_service
            .now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();

        delay_for(wait_time).await;

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
    #[actix_rt::test]
    async fn lowres_time_service_time_updates_after_resolution_interval() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(150);
        let time_service = LowResTimeService::with(resolution);

        let first_time = time_service.now();

        delay_for(wait_time).await;

        let second_time = time_service.now();
        assert!(second_time - first_time >= wait_time);
    }
}
