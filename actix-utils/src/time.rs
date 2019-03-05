use std::time::{self, Duration, Instant};

use actix_service::{NewService, Service, Void};
use futures::future::{ok, FutureResult};
use futures::{Async, Future, Poll};
use tokio_timer::sleep;

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

impl NewService<()> for LowResTime {
    type Response = Instant;
    type Error = Void;
    type InitError = Void;
    type Service = LowResTimeService;
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &()) -> Self::Future {
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

            tokio_current_thread::spawn(sleep(interval).map_err(|_| panic!()).and_then(
                move |_| {
                    inner.get_mut().current.take();
                    Ok(())
                },
            ));
            now
        }
    }
}

impl Service<()> for LowResTimeService {
    type Response = Instant;
    type Error = Void;
    type Future = FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
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

            tokio_current_thread::spawn(sleep(interval).map_err(|_| panic!()).and_then(
                move |_| {
                    inner.get_mut().current.take();
                    Ok(())
                },
            ));
            now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future;
    use std::time::{Duration, SystemTime};

    /// State Under Test: Two calls of `SystemTimeService::now()` return the same value if they are done within resolution interval of `SystemTimeService`.
    ///
    /// Expected Behavior: Two back-to-back calls of `SystemTimeService::now()` return the same value.
    #[test]
    fn system_time_service_time_does_not_immediately_change() {
        let resolution = Duration::from_millis(50);

        let _ = actix_rt::System::new("test").block_on(future::lazy(|| {
            let time_service = SystemTimeService::with(resolution);

            assert_eq!(time_service.now(), time_service.now());

            Ok::<(), ()>(())
        }));
    }

    /// State Under Test: Two calls of `LowResTimeService::now()` return the same value if they are done within resolution interval of `SystemTimeService`.
    ///
    /// Expected Behavior: Two back-to-back calls of `LowResTimeService::now()` return the same value.
    #[test]
    fn lowres_time_service_time_does_not_immediately_change() {
        let resolution = Duration::from_millis(50);

        let _ = actix_rt::System::new("test").block_on(future::lazy(|| {
            let time_service = LowResTimeService::with(resolution);

            assert_eq!(time_service.now(), time_service.now());

            Ok::<(), ()>(())
        }));
    }

    /// State Under Test: `SystemTimeService::now()` updates returned value every resolution period.
    ///
    /// Expected Behavior: Two calls of `LowResTimeService::now()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a resolution interval.
    #[test]
    fn system_time_service_time_updates_after_resolution_interval() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(150);

        let _ = actix_rt::System::new("test").block_on(future::lazy(|| {
            let time_service = SystemTimeService::with(resolution);

            let first_time = time_service
                .now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap();

            sleep(wait_time).then(move |_| {
                let second_time = time_service
                    .now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap();

                assert!(second_time - first_time >= wait_time);

                Ok::<(), ()>(())
            })
        }));
    }

    /// State Under Test: `LowResTimeService::now()` updates returned value every resolution period.
    ///
    /// Expected Behavior: Two calls of `LowResTimeService::now()` made in subsequent resolution interval return different values
    /// and second value is greater than the first one at least by a resolution interval.
    #[test]
    fn lowres_time_service_time_updates_after_resolution_interval() {
        let resolution = Duration::from_millis(100);
        let wait_time = Duration::from_millis(150);

        let _ = actix_rt::System::new("test").block_on(future::lazy(|| {
            let time_service = LowResTimeService::with(resolution);

            let first_time = time_service.now();

            sleep(wait_time).then(move |_| {
                let second_time = time_service.now();

                assert!(second_time - first_time >= wait_time);

                Ok::<(), ()>(())
            })
        }));
    }
}
