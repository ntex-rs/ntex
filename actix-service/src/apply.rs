use std::marker::PhantomData;

use futures::{Async, Future, IntoFuture, Poll};

use super::{IntoNewService, IntoService, NewService, Service};

/// `Apply` service combinator
pub struct Apply<T, F, In, Out>
where
    T: Service,
{
    service: T,
    f: F,
    r: PhantomData<(In, Out)>,
}

impl<T, F, In, Out> Apply<T, F, In, Out>
where
    T: Service,
    F: FnMut(In, &mut T) -> Out,
    Out: IntoFuture,
    Out::Error: From<T::Error>,
{
    /// Create new `Apply` combinator
    pub fn new<I: IntoService<T>>(service: I, f: F) -> Self {
        Self {
            service: service.into_service(),
            f,
            r: PhantomData,
        }
    }
}

impl<T, F, In, Out> Clone for Apply<T, F, In, Out>
where
    T: Service + Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Apply {
            service: self.service.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<T, F, In, Out> Service for Apply<T, F, In, Out>
where
    T: Service,
    F: FnMut(In, &mut T) -> Out,
    Out: IntoFuture,
    Out::Error: From<T::Error>,
{
    type Request = In;
    type Response = Out::Item;
    type Error = Out::Error;
    type Future = Out::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(|e| e.into())
    }

    fn call(&mut self, req: In) -> Self::Future {
        (self.f)(req, &mut self.service).into_future()
    }
}

/// `ApplyNewService` new service combinator
pub struct ApplyNewService<T, F, In, Out>
where
    T: NewService,
{
    service: T,
    f: F,
    r: PhantomData<(In, Out)>,
}

impl<T, F, In, Out> ApplyNewService<T, F, In, Out>
where
    T: NewService,
    F: FnMut(In, &mut T::Service) -> Out + Clone,
    Out: IntoFuture,
    Out::Error: From<T::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub fn new<F1: IntoNewService<T>>(service: F1, f: F) -> Self {
        Self {
            f,
            service: service.into_new_service(),
            r: PhantomData,
        }
    }
}

impl<T, F, In, Out> Clone for ApplyNewService<T, F, In, Out>
where
    T: NewService + Clone,
    F: FnMut(In, &mut T::Service) -> Out + Clone,
    Out: IntoFuture,
{
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<T, F, In, Out> NewService for ApplyNewService<T, F, In, Out>
where
    T: NewService,
    F: FnMut(In, &mut T::Service) -> Out + Clone,
    Out: IntoFuture,
    Out::Error: From<T::Error>,
{
    type Request = In;
    type Response = Out::Item;
    type Error = Out::Error;
    type Service = Apply<T::Service, F, In, Out>;

    type InitError = T::InitError;
    type Future = ApplyNewServiceFuture<T, F, In, Out>;

    fn new_service(&self) -> Self::Future {
        ApplyNewServiceFuture::new(self.service.new_service(), self.f.clone())
    }
}

pub struct ApplyNewServiceFuture<T, F, In, Out>
where
    T: NewService,
    F: FnMut(In, &mut T::Service) -> Out + Clone,
    Out: IntoFuture,
{
    fut: T::Future,
    f: Option<F>,
    r: PhantomData<(In, Out)>,
}

impl<T, F, In, Out> ApplyNewServiceFuture<T, F, In, Out>
where
    T: NewService,
    F: FnMut(In, &mut T::Service) -> Out + Clone,
    Out: IntoFuture,
{
    fn new(fut: T::Future, f: F) -> Self {
        ApplyNewServiceFuture {
            f: Some(f),
            fut,
            r: PhantomData,
        }
    }
}

impl<T, F, In, Out> Future for ApplyNewServiceFuture<T, F, In, Out>
where
    T: NewService,
    F: FnMut(In, &mut T::Service) -> Out + Clone,
    Out: IntoFuture,
    Out::Error: From<T::Error>,
{
    type Item = Apply<T::Service, F, In, Out>;
    type Error = T::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Async::Ready(service) = self.fut.poll()? {
            Ok(Async::Ready(Apply::new(service, self.f.take().unwrap())))
        } else {
            Ok(Async::NotReady)
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::future::{ok, FutureResult};
    use futures::{Async, Future, Poll};

    use crate::{IntoNewService, IntoService, NewService, Service, ServiceExt};

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
    fn test_call() {
        let blank = |req| Ok(req);

        let mut srv = blank.into_service().apply(Srv, |req: &'static str, srv| {
            srv.call(()).map(move |res| (req, res))
        });
        assert!(srv.poll_ready().is_ok());
        let res = srv.call("srv").poll();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), Async::Ready(("srv", ())));
    }

    #[test]
    fn test_new_service() {
        let blank = || Ok::<_, ()>((|req| Ok(req)).into_service());

        let new_srv = blank.into_new_service().apply(
            || Ok(Srv),
            |req: &'static str, srv| srv.call(()).map(move |res| (req, res)),
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
}
