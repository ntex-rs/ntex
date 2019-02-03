use futures::{try_ready, Async, Future, IntoFuture, Poll};

use super::{FnNewTransform, FnTransform};
use super::{
    IntoNewService, IntoNewTransform, IntoService, IntoTransform, NewService, NewTransform,
    Service, Transform,
};

/// `Apply` service combinator
pub struct Apply<T, S>
where
    T: Transform<S>,
    T::Error: From<S::Error>,
    S: Service,
{
    transform: T,
    service: S,
}

impl<T, S> Apply<T, S>
where
    T: Transform<S>,
    T::Error: From<S::Error>,
    S: Service,
{
    /// Create new `Apply` combinator
    pub fn new<T1: IntoTransform<T, S>, S1: IntoService<S>>(
        transform: T1,
        service: S1,
    ) -> Self {
        Self {
            transform: transform.into_transform(),
            service: service.into_service(),
        }
    }
}

impl<F, S, Req, Out> Apply<FnTransform<F, S, Req, Out>, S>
where
    F: FnMut(Req, &mut S) -> Out,
    Out: IntoFuture,
    Out::Error: From<S::Error>,
    S: Service,
{
    /// Create new `Apply` combinator
    pub fn new_fn<S1: IntoService<S>>(service: S1, transform: F) -> Self {
        Self {
            service: service.into_service(),
            transform: transform.into_transform(),
        }
    }
}

impl<T, S> Clone for Apply<T, S>
where
    S: Service + Clone,
    T::Error: From<S::Error>,
    T: Transform<S> + Clone,
{
    fn clone(&self) -> Self {
        Apply {
            service: self.service.clone(),
            transform: self.transform.clone(),
        }
    }
}

impl<T, S> Service for Apply<T, S>
where
    T: Transform<S>,
    T::Error: From<S::Error>,
    S: Service,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = T::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        try_ready!(self.service.poll_ready());
        self.transform.poll_ready().map_err(|e| e.into())
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        self.transform.call(req, &mut self.service).into_future()
    }
}

/// `ApplyNewService` new service combinator
pub struct ApplyNewService<T, S>
where
    // T::InitError: From<S::InitError>,
    T: NewTransform<S::Service, InitError = S::InitError>,
    T::Error: From<S::Error>,
    S: NewService,
{
    transform: T,
    service: S,
}

impl<T, S> ApplyNewService<T, S>
where
    T: NewTransform<S::Service, InitError = S::InitError>,
    T::Error: From<S::Error>,
    S: NewService,
{
    /// Create new `ApplyNewService` new service instance
    pub fn new<T1: IntoNewTransform<T, S::Service>, S1: IntoNewService<S>>(
        transform: T1,
        service: S1,
    ) -> Self {
        Self {
            transform: transform.into_new_transform(),
            service: service.into_new_service(),
        }
    }
}

impl<F, S, In, Out> ApplyNewService<FnNewTransform<F, S::Service, In, Out, S::InitError>, S>
where
    F: FnMut(In, &mut S::Service) -> Out + Clone,
    Out: IntoFuture,
    Out::Error: From<S::Error>,
    S: NewService,
{
    /// Create new `Apply` combinator factory
    pub fn new_fn<S1: IntoNewService<S>>(service: S1, transform: F) -> Self {
        Self {
            service: service.into_new_service(),
            transform: FnNewTransform::new(transform),
        }
    }
}

impl<T, S> Clone for ApplyNewService<T, S>
where
    T: NewTransform<S::Service, InitError = S::InitError> + Clone,
    T::Error: From<S::Error>,
    S: NewService + Clone,
{
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            transform: self.transform.clone(),
        }
    }
}

impl<T, S> NewService for ApplyNewService<T, S>
where
    T: NewTransform<S::Service, InitError = S::InitError>,
    T::Error: From<S::Error>,
    S: NewService,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Service = Apply<T::Transform, S::Service>;

    type InitError = T::InitError;
    type Future = ApplyNewServiceFuture<T, S>;

    fn new_service(&self) -> Self::Future {
        ApplyNewServiceFuture {
            fut_t: self.transform.new_transform(),
            fut_s: self.service.new_service(),
            service: None,
            transform: None,
        }
    }
}

pub struct ApplyNewServiceFuture<T, S>
where
    T: NewTransform<S::Service, InitError = S::InitError>,
    T::Error: From<S::Error>,
    S: NewService,
{
    fut_s: S::Future,
    fut_t: T::Future,
    service: Option<S::Service>,
    transform: Option<T::Transform>,
}

impl<T, S> Future for ApplyNewServiceFuture<T, S>
where
    T: NewTransform<S::Service, InitError = S::InitError>,
    T::Error: From<S::Error>,
    S: NewService,
{
    type Item = Apply<T::Transform, S::Service>;
    type Error = T::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.transform.is_none() {
            if let Async::Ready(transform) = self.fut_t.poll()? {
                self.transform = Some(transform);
            }
        }

        if self.service.is_none() {
            if let Async::Ready(service) = self.fut_s.poll()? {
                self.service = Some(service);
            }
        }

        if self.transform.is_some() && self.service.is_some() {
            Ok(Async::Ready(Apply {
                service: self.service.take().unwrap(),
                transform: self.transform.take().unwrap(),
            }))
        } else {
            Ok(Async::NotReady)
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::future::{ok, FutureResult};
    use futures::{Async, Future, Poll};

    use super::*;
    use crate::{NewService, Service};

    #[derive(Clone)]
    struct Srv;
    impl Service for Srv {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = FutureResult<(), ()>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(Async::Ready(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            ok(())
        }
    }

    #[test]
    fn test_apply() {
        let mut srv = Apply::new_fn(
            |req: &'static str, srv| srv.call(()).map(move |res| (req, res)),
            Srv,
        );
        assert!(srv.poll_ready().is_ok());
        let res = srv.call("srv").poll();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), Async::Ready(("srv", ())));
    }

    #[test]
    fn test_new_service() {
        let new_srv = ApplyNewService::new(
            |req: &'static str, srv: &mut Srv| srv.call(()).map(move |res| (req, res)),
            || Ok::<_, ()>(Srv),
        );
        if let Async::Ready(mut srv) = new_srv.new_service().poll().unwrap() {
            assert!(srv.poll_ready().is_ok());
            let res = srv.call("srv").poll();
            assert!(res.is_ok());
            assert_eq!(res.unwrap(), Async::Ready(("srv", ())));
        } else {
            panic!()
        }
    }

    // #[test]
    // fn test_new_service_fn() {
    //     let new_srv = ApplyNewService::new_fn(
    //         || Ok(Srv),
    //         |req: &'static str, srv| srv.call(()).map(move |res| (req, res)),
    //     );
    //     if let Async::Ready(mut srv) = new_srv.new_service().poll().unwrap() {
    //         assert!(srv.poll_ready().is_ok());
    //         let res = srv.call("srv").poll();
    //         assert!(res.is_ok());
    //         assert_eq!(res.unwrap(), Async::Ready(("srv", ())));
    //     } else {
    //         panic!()
    //     }
    // }
}
