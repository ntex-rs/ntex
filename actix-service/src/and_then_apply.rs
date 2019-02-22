use futures::{Async, Future, Poll};

use super::{NewService, NewTransform, Service, Transform};
use crate::cell::Cell;

/// `Apply` service combinator
pub struct AndThenTransform<T, A, B>
where
    A: Service,
    B: Service<Error = A::Error>,
    T: Transform<B, Request = A::Response>,
    T::Error: From<A::Error>,
{
    a: A,
    b: Cell<B>,
    t: Cell<T>,
}

impl<T, A, B> AndThenTransform<T, A, B>
where
    A: Service,
    B: Service<Error = A::Error>,
    T: Transform<B, Request = A::Response>,
    T::Error: From<A::Error>,
{
    /// Create new `Apply` combinator
    pub fn new(t: T, a: A, b: B) -> Self {
        Self {
            a,
            b: Cell::new(b),
            t: Cell::new(t),
        }
    }
}

impl<T, A, B> Clone for AndThenTransform<T, A, B>
where
    A: Service + Clone,
    B: Service<Error = A::Error>,
    T: Transform<B, Request = A::Response>,
    T::Error: From<A::Error>,
{
    fn clone(&self) -> Self {
        AndThenTransform {
            a: self.a.clone(),
            b: self.b.clone(),
            t: self.t.clone(),
        }
    }
}

impl<T, A, B> Service for AndThenTransform<T, A, B>
where
    A: Service,
    B: Service<Error = A::Error>,
    T: Transform<B, Request = A::Response>,
    T::Error: From<A::Error>,
{
    type Request = A::Request;
    type Response = T::Response;
    type Error = T::Error;
    type Future = AndThenTransformFuture<T, A, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let notready = Async::NotReady == self.a.poll_ready()?;
        let notready = Async::NotReady == self.b.get_mut().poll_ready()? || notready;
        let notready = Async::NotReady == self.t.get_mut().poll_ready()? || notready;

        if notready {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(()))
        }
    }

    fn call(&mut self, req: A::Request) -> Self::Future {
        AndThenTransformFuture {
            b: self.b.clone(),
            t: self.t.clone(),
            fut_t: None,
            fut_a: Some(self.a.call(req)),
        }
    }
}

pub struct AndThenTransformFuture<T, A, B>
where
    A: Service,
    B: Service<Error = A::Error>,
    T: Transform<B, Request = A::Response>,
    T::Error: From<A::Error>,
{
    b: Cell<B>,
    t: Cell<T>,
    fut_a: Option<A::Future>,
    fut_t: Option<T::Future>,
}

impl<T, A, B> Future for AndThenTransformFuture<T, A, B>
where
    A: Service,
    B: Service<Error = A::Error>,
    T: Transform<B, Request = A::Response>,
    T::Error: From<A::Error>,
{
    type Item = T::Response;
    type Error = T::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut fut) = self.fut_t {
            return fut.poll();
        }

        match self.fut_a.as_mut().expect("Bug in actix-service").poll() {
            Ok(Async::Ready(resp)) => {
                let _ = self.fut_a.take();
                self.fut_t = Some(self.t.get_mut().call(resp, self.b.get_mut()));
                self.poll()
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err.into()),
        }
    }
}

/// `Apply` new service combinator
pub struct AndThenTransformNewService<T, A, B, C> {
    a: A,
    b: B,
    t: T,
    _t: std::marker::PhantomData<C>,
}

impl<T, A, B, C> AndThenTransformNewService<T, A, B, C>
where
    A: NewService<C>,
    B: NewService<C, Error = A::Error, InitError = A::InitError>,
    T: NewTransform<B::Service, Request = A::Response, InitError = A::InitError>,
    T::Error: From<A::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub fn new(t: T, a: A, b: B) -> Self {
        Self {
            a,
            b,
            t,
            _t: std::marker::PhantomData,
        }
    }
}

impl<T, A, B, C> Clone for AndThenTransformNewService<T, A, B, C>
where
    A: Clone,
    B: Clone,
    T: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
            t: self.t.clone(),
            _t: std::marker::PhantomData,
        }
    }
}

impl<T, A, B, C> NewService<C> for AndThenTransformNewService<T, A, B, C>
where
    A: NewService<C>,
    B: NewService<C, Error = A::Error, InitError = A::InitError>,
    T: NewTransform<B::Service, Request = A::Response, InitError = A::InitError>,
    T::Error: From<A::Error>,
{
    type Request = A::Request;
    type Response = T::Response;
    type Error = T::Error;

    type InitError = T::InitError;
    type Service = AndThenTransform<T::Transform, A::Service, B::Service>;
    type Future = AndThenTransformNewServiceFuture<T, A, B, C>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        AndThenTransformNewServiceFuture {
            a: None,
            b: None,
            t: None,
            fut_a: self.a.new_service(cfg),
            fut_b: self.b.new_service(cfg),
            fut_t: self.t.new_transform(),
        }
    }
}

pub struct AndThenTransformNewServiceFuture<T, A, B, C>
where
    A: NewService<C>,
    B: NewService<C, Error = A::Error, InitError = A::InitError>,
    T: NewTransform<B::Service, Request = A::Response, InitError = A::InitError>,
    T::Error: From<A::Error>,
{
    fut_b: B::Future,
    fut_a: A::Future,
    fut_t: T::Future,
    a: Option<A::Service>,
    b: Option<B::Service>,
    t: Option<T::Transform>,
}

impl<T, A, B, C> Future for AndThenTransformNewServiceFuture<T, A, B, C>
where
    A: NewService<C>,
    B: NewService<C, Error = A::Error, InitError = A::InitError>,
    T: NewTransform<B::Service, Request = A::Response, InitError = A::InitError>,
    T::Error: From<A::Error>,
{
    type Item = AndThenTransform<T::Transform, A::Service, B::Service>;
    type Error = T::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.t.is_none() {
            if let Async::Ready(transform) = self.fut_t.poll()? {
                self.t = Some(transform);
            }
        }

        if self.a.is_none() {
            if let Async::Ready(service) = self.fut_a.poll()? {
                self.a = Some(service);
            }
        }

        if self.b.is_none() {
            if let Async::Ready(service) = self.fut_b.poll()? {
                self.b = Some(service);
            }
        }

        if self.a.is_some() && self.b.is_some() && self.t.is_some() {
            Ok(Async::Ready(AndThenTransform {
                a: self.a.take().unwrap(),
                t: Cell::new(self.t.take().unwrap()),
                b: Cell::new(self.b.take().unwrap()),
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
    fn test_apply() {
        let blank = |req| Ok(req);

        let mut srv = blank.into_service().apply(
            |req: &'static str, srv: &mut Srv| srv.call(()).map(move |res| (req, res)),
            Srv,
        );
        assert!(srv.poll_ready().is_ok());
        let res = srv.call("srv").poll();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), Async::Ready(("srv", ())));
    }

    #[test]
    fn test_new_service() {
        let blank = || Ok::<_, ()>((|req| Ok(req)).into_service());

        let new_srv = blank.into_new_service().apply(
            |req: &'static str, srv: &mut Srv| srv.call(()).map(move |res| (req, res)),
            || Ok(Srv),
        );
        if let Async::Ready(mut srv) = new_srv.new_service(&()).poll().unwrap() {
            assert!(srv.poll_ready().is_ok());
            let res = srv.call("srv").poll();
            assert!(res.is_ok());
            assert_eq!(res.unwrap(), Async::Ready(("srv", ())));
        } else {
            panic!()
        }
    }
}
