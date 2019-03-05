use actix_service::{Service, Transform, Void};
use futures::future::{ok, FutureResult};
use futures::{Async, Future, Poll};

use super::counter::{Counter, CounterGuard};

/// InFlight - new service for service that can limit number of in-flight
/// async requests.
///
/// Default number of in-flight requests is 15
pub struct InFlight {
    max_inflight: usize,
}

impl InFlight {
    pub fn new(max: usize) -> Self {
        Self { max_inflight: max }
    }
}

impl Default for InFlight {
    fn default() -> Self {
        Self::new(15)
    }
}

impl<S: Service<R>, R> Transform<S, R> for InFlight {
    type Response = S::Response;
    type Error = S::Error;
    type InitError = Void;
    type Transform = InFlightService<S>;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(InFlightService::new(self.max_inflight, service))
    }
}

pub struct InFlightService<S> {
    count: Counter,
    service: S,
}

impl<S> InFlightService<S> {
    pub fn new(max: usize, service: S) -> Self {
        Self {
            service,
            count: Counter::new(max),
        }
    }
}

impl<T, R> Service<R> for InFlightService<T>
where
    T: Service<R>,
{
    type Response = T::Response;
    type Error = T::Error;
    type Future = InFlightServiceResponse<T, R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()?;

        if !self.count.available() {
            log::trace!("InFlight limit exceeded");
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(()))
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        InFlightServiceResponse {
            fut: self.service.call(req),
            _guard: self.count.get(),
        }
    }
}

#[doc(hidden)]
pub struct InFlightServiceResponse<T: Service<R>, R> {
    fut: T::Future,
    _guard: CounterGuard,
}

impl<T: Service<R>, R> Future for InFlightServiceResponse<T, R> {
    type Item = T::Response;
    type Error = T::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        self.fut.poll()
    }
}

#[cfg(test)]
mod tests {
    use futures::future::lazy;
    use futures::{Async, Poll};

    use std::time::Duration;

    use super::*;
    use actix_service::blank::{Blank, BlankNewService};
    use actix_service::{NewService, Service, ServiceExt};

    struct SleepService(Duration);

    impl Service<()> for SleepService {
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
    fn test_transform() {
        let wait_time = Duration::from_millis(50);
        let _ = actix_rt::System::new("test").block_on(lazy(|| {
            let mut srv =
                Blank::new().and_then(InFlightService::new(1, SleepService(wait_time)));
            assert_eq!(srv.poll_ready(), Ok(Async::Ready(())));

            let mut res = srv.call(());
            let _ = res.poll();
            assert_eq!(srv.poll_ready(), Ok(Async::NotReady));

            drop(res);
            assert_eq!(srv.poll_ready(), Ok(Async::Ready(())));

            Ok::<_, ()>(())
        }));
    }

    #[test]
    fn test_newtransform() {
        let wait_time = Duration::from_millis(50);
        let _ = actix_rt::System::new("test").block_on(lazy(|| {
            let srv =
                BlankNewService::new().apply(InFlight::new(1), || Ok(SleepService(wait_time)));

            if let Async::Ready(mut srv) = srv.new_service(&()).poll().unwrap() {
                assert_eq!(srv.poll_ready(), Ok(Async::Ready(())));

                let mut res = srv.call(());
                let _ = res.poll();
                assert_eq!(srv.poll_ready(), Ok(Async::NotReady));

                drop(res);
                assert_eq!(srv.poll_ready(), Ok(Async::Ready(())));
            } else {
                panic!()
            }

            Ok::<_, ()>(())
        }));
    }
}
