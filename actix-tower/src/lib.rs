//! Utilities to ease interoperability with services based on the `tower-service` crate.

use actix_service::Service as ActixService;
use std::marker::PhantomData;
use tower_service::Service as TowerService;

/// Compatibility wrapper associating a `tower_service::Service` with a particular
/// `Request` type, so that it can be used as an `actix_service::Service`.
pub struct TowerCompat<S, R> {
    inner: S,
    _phantom: PhantomData<R>,
}

impl<S, R> TowerCompat<S, R> {
    /// Wraps a `tower_service::Service` in a compatibility wrapper.
    pub fn new(inner: S) -> Self {
        TowerCompat {
            inner,
            _phantom: PhantomData,
        }
    }
}

/// Extension trait for wrapping `tower_service::Service` instances for use as
/// an `actix_service::Service`.
pub trait TowerServiceExt {
    /// Wraps a `tower_service::Service` in a compatibility wrapper.
    fn compat<R>(self) -> TowerCompat<Self, R>
    where
        Self: TowerService<R> + Sized;
}

impl<S> TowerServiceExt for S {
    fn compat<R>(self) -> TowerCompat<Self, R>
    where
        Self: TowerService<R> + Sized
    {
        TowerCompat::new(self)
    }
}

impl<S, R> ActixService for TowerCompat<S, R>
where
    S: TowerService<R>,
{
    type Request = R;
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
        TowerService::poll_ready(&mut self.inner)
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        TowerService::call(&mut self.inner, req)
    }
}

#[cfg(test)]
mod tests {
    use super::TowerServiceExt;
    use actix_service::{Service as ActixService, ServiceExt, Transform};
    use futures::{future::FutureResult, Async, Poll, Future};
    use tower_service::Service as TowerService;

    struct RandomService;
    impl<R> TowerService<R> for RandomService {
        type Response = u32;
        type Error = ();
        type Future = FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(Async::Ready(()))
        }

        fn call(&mut self, _req: R) -> Self::Future {
            futures::finished(4)
        }
    }

    #[test]
    fn tower_service_as_actix_service_returns_4() {
        let mut s = RandomService.compat();

        assert_eq!(Ok(Async::Ready(())), s.poll_ready());

        assert_eq!(Ok(Async::Ready(4)), s.call(()).poll());
    }

    #[test]
    fn tower_service_as_actix_service_can_combine() {
        let mut s = RandomService.compat().map(|x| x + 1);

        assert_eq!(Ok(Async::Ready(())), s.poll_ready());

        assert_eq!(Ok(Async::Ready(5)), s.call(()).poll());
    }

    struct AddOneService;
    impl TowerService<u32> for AddOneService {
        type Response = u32;
        type Error = ();
        type Future = FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(Async::Ready(()))
        }

        fn call(&mut self, req: u32) -> Self::Future {
            futures::finished(req + 1)
        }
    }

    #[test]
    fn tower_services_as_actix_services_chained() {
        let s1 = RandomService.compat();
        let s2 = AddOneService.compat();
        let s3 = AddOneService.compat();

        let mut s = s1.and_then(s2).and_then(s3);

        assert_eq!(Ok(Async::Ready(())), s.poll_ready());

        assert_eq!(Ok(Async::Ready(6)), s.call(()).poll());
    }

    #[test]
    fn tower_services_as_actix_services_chained_2() {
        let s1 = RandomService.compat();
        let s2 = AddOneService.compat();
        let s3 = AddOneService.compat();
        let s4 = RandomService.compat();

        let mut s = s1.and_then(s2).and_then(s3).and_then(s4);

        assert_eq!(Ok(Async::Ready(())), s.poll_ready());

        assert_eq!(Ok(Async::Ready(4)), s.call(()).poll());
    }

    struct DoMathTransform<S>(S);
    impl<S> ActixService for DoMathTransform<S>
    where
        S: ActixService<Response = u32>,
        S::Future: 'static,
    {
        type Request = S::Request;
        type Response = u32;
        type Error = S::Error;
        type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.0.poll_ready()
        }

        fn call(&mut self, req: Self::Request) -> Self::Future {
            let fut = self.0.call(req).map(|x| x * 17);
            Box::new(fut)
        }
    }

    struct DoMath;
    impl<S> Transform<S> for DoMath
    where
        S: ActixService<Response = u32>,
        S::Future: 'static,
    {
        type Request = S::Request;
        type Response = u32;
        type Error = S::Error;
        type Transform = DoMathTransform<S>;
        type InitError = ();
        type Future = FutureResult<Self::Transform, Self::InitError>;

        fn new_transform(&self, service: S) -> Self::Future {
            futures::finished(DoMathTransform(service))
        }
    }

    #[test]
    fn tower_service_as_actix_service_can_be_transformed() {
        let transform = DoMath;

        let mut s = transform.new_transform(RandomService.compat()).wait().unwrap();

        assert_eq!(Ok(Async::Ready(())), s.poll_ready());

        assert_eq!(Ok(Async::Ready(68)), s.call(()).poll());
    }
}