use std::marker::PhantomData;

use futures::{Async, Future, IntoFuture, Poll};

use super::{IntoNewService, IntoService, NewService, Service};
use crate::cell::Cell;

/// `Apply` service combinator
pub struct AndThenApply<A, B, F, Out>
where
    A: Service,
    B: Service<Error = A::Error>,
    F: FnMut(A::Response, &mut B) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    a: A,
    b: Cell<B>,
    f: Cell<F>,
    r: PhantomData<(Out,)>,
}

impl<A, B, F, Out> AndThenApply<A, B, F, Out>
where
    A: Service,
    B: Service<Error = A::Error>,
    F: FnMut(A::Response, &mut B) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    /// Create new `Apply` combinator
    pub fn new<A1: IntoService<A>, B1: IntoService<B>>(a: A1, b: B1, f: F) -> Self {
        Self {
            f: Cell::new(f),
            a: a.into_service(),
            b: Cell::new(b.into_service()),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Out> Clone for AndThenApply<A, B, F, Out>
where
    A: Service + Clone,
    B: Service<Error = A::Error>,
    F: FnMut(A::Response, &mut B) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    fn clone(&self) -> Self {
        AndThenApply {
            a: self.a.clone(),
            b: self.b.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Out> Service for AndThenApply<A, B, F, Out>
where
    A: Service,
    B: Service<Error = A::Error>,
    F: FnMut(A::Response, &mut B) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    type Request = A::Request;
    type Response = Out::Item;
    type Error = A::Error;
    type Future = AndThenApplyFuture<A, B, F, Out>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        let not_ready = self.a.poll_ready()?.is_not_ready();
        if self.b.get_mut().poll_ready()?.is_not_ready() || not_ready {
            Ok(Async::NotReady)
        } else {
            Ok(Async::Ready(()))
        }
    }

    fn call(&mut self, req: A::Request) -> Self::Future {
        AndThenApplyFuture {
            b: self.b.clone(),
            f: self.f.clone(),
            fut_b: None,
            fut_a: Some(self.a.call(req)),
        }
    }
}

pub struct AndThenApplyFuture<A, B, F, Out>
where
    A: Service,
    B: Service<Error = A::Error>,
    F: FnMut(A::Response, &mut B) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    b: Cell<B>,
    f: Cell<F>,
    fut_a: Option<A::Future>,
    fut_b: Option<Out::Future>,
}

impl<A, B, F, Out> Future for AndThenApplyFuture<A, B, F, Out>
where
    A: Service,
    B: Service<Error = A::Error>,
    F: FnMut(A::Response, &mut B) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    type Item = Out::Item;
    type Error = A::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if let Some(ref mut fut) = self.fut_b {
            return fut.poll().map_err(|e| e.into());
        }

        match self.fut_a.as_mut().expect("Bug in actix-service").poll() {
            Ok(Async::Ready(resp)) => {
                let _ = self.fut_a.take();
                self.fut_b =
                    Some((&mut *self.f.get_mut())(resp, self.b.get_mut()).into_future());
                self.poll()
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err),
        }
    }
}

/// `ApplyNewService` new service combinator
pub struct AndThenApplyNewService<A, B, F, Out> {
    a: A,
    b: B,
    f: Cell<F>,
    r: PhantomData<Out>,
}

impl<A, B, F, Out> AndThenApplyNewService<A, B, F, Out>
where
    A: NewService,
    B: NewService<Config = A::Config, Error = A::Error, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    /// Create new `ApplyNewService` new service instance
    pub fn new<A1: IntoNewService<A>, B1: IntoNewService<B>>(a: A1, b: B1, f: F) -> Self {
        Self {
            f: Cell::new(f),
            a: a.into_new_service(),
            b: b.into_new_service(),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Out> Clone for AndThenApplyNewService<A, B, F, Out>
where
    A: Clone,
    B: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<A, B, F, Out> NewService for AndThenApplyNewService<A, B, F, Out>
where
    A: NewService,
    B: NewService<Config = A::Config, Error = A::Error, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    type Request = A::Request;
    type Response = Out::Item;
    type Error = A::Error;
    type Service = AndThenApply<A::Service, B::Service, F, Out>;
    type Config = A::Config;
    type InitError = A::InitError;
    type Future = AndThenApplyNewServiceFuture<A, B, F, Out>;

    fn new_service(&self, cfg: &A::Config) -> Self::Future {
        AndThenApplyNewServiceFuture {
            a: None,
            b: None,
            f: self.f.clone(),
            fut_a: self.a.new_service(cfg).into_future(),
            fut_b: self.b.new_service(cfg).into_future(),
        }
    }
}

pub struct AndThenApplyNewServiceFuture<A, B, F, Out>
where
    A: NewService,
    B: NewService<Error = A::Error, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    fut_b: B::Future,
    fut_a: A::Future,
    f: Cell<F>,
    a: Option<A::Service>,
    b: Option<B::Service>,
}

impl<A, B, F, Out> Future for AndThenApplyNewServiceFuture<A, B, F, Out>
where
    A: NewService,
    B: NewService<Error = A::Error, InitError = A::InitError>,
    F: FnMut(A::Response, &mut B::Service) -> Out,
    Out: IntoFuture,
    Out::Error: Into<A::Error>,
{
    type Item = AndThenApply<A::Service, B::Service, F, Out>;
    type Error = A::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
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

        if self.a.is_some() && self.b.is_some() {
            Ok(Async::Ready(AndThenApply {
                f: self.f.clone(),
                a: self.a.take().unwrap(),
                b: Cell::new(self.b.take().unwrap()),
                r: PhantomData,
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

    use crate::blank::{Blank, BlankNewService};
    use crate::{NewService, Service, ServiceExt};

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
        let mut srv = Blank::new().apply_fn(Srv, |req: &'static str, srv| {
            srv.call(()).map(move |res| (req, res))
        });
        assert!(srv.poll_ready().is_ok());
        let res = srv.call("srv").poll();
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), Async::Ready(("srv", ())));
    }

    #[test]
    fn test_new_service() {
        let new_srv = BlankNewService::new_unit().apply_fn(
            || Ok(Srv),
            |req: &'static str, srv| srv.call(()).map(move |res| (req, res)),
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
