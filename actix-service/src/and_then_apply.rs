use std::marker::PhantomData;
use std::rc::Rc;

use futures::{Async, Future, Poll};

use crate::and_then::AndThen;
use crate::from_err::FromErr;
use crate::{NewService, Transform};

/// `Apply` new service combinator
pub struct AndThenTransform<T, A, B, BR> {
    a: A,
    b: B,
    t: Rc<T>,
    _t: PhantomData<BR>,
}

impl<T, A, B, BR> AndThenTransform<T, A, B, BR> {
    /// Create new `ApplyNewService` new service instance
    pub fn new<AR, C>(t: T, a: A, b: B) -> Self
    where
        A: NewService<AR, C>,
        B: NewService<BR, C, InitError = A::InitError>,
        T: Transform<B::Service, A::Response, InitError = A::InitError>,
        T::Error: From<A::Error>,
    {
        Self {
            a,
            b,
            t: Rc::new(t),
            _t: std::marker::PhantomData,
        }
    }
}

impl<T, A, B, BR> Clone for AndThenTransform<T, A, B, BR>
where
    A: Clone,
    B: Clone,
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

impl<T, A, B, AR, BR, C> NewService<AR, C> for AndThenTransform<T, A, B, BR>
where
    A: NewService<AR, C>,
    B: NewService<BR, C, InitError = A::InitError>,
    T: Transform<B::Service, A::Response, InitError = A::InitError>,
    T::Error: From<A::Error>,
{
    type Response = T::Response;
    type Error = T::Error;

    type InitError = T::InitError;
    type Service = AndThen<FromErr<A::Service, T::Error>, T::Transform>;
    type Future = AndThenTransformFuture<T, A, B, AR, BR, C>;

    fn new_service(&self, cfg: &C) -> Self::Future {
        AndThenTransformFuture {
            a: None,
            t: None,
            t_cell: self.t.clone(),
            fut_a: self.a.new_service(cfg),
            fut_b: self.b.new_service(cfg),
            fut_t: None,
        }
    }
}

pub struct AndThenTransformFuture<T, A, B, AR, BR, C>
where
    A: NewService<AR, C>,
    B: NewService<BR, C, InitError = A::InitError>,
    T: Transform<B::Service, A::Response, InitError = A::InitError>,
    T::Error: From<A::Error>,
{
    fut_a: A::Future,
    fut_b: B::Future,
    fut_t: Option<T::Future>,
    a: Option<A::Service>,
    t: Option<T::Transform>,
    t_cell: Rc<T>,
}

impl<T, A, B, AR, BR, C> Future for AndThenTransformFuture<T, A, B, AR, BR, C>
where
    A: NewService<AR, C>,
    B: NewService<BR, C, InitError = A::InitError>,
    T: Transform<B::Service, A::Response, InitError = A::InitError>,
    T::Error: From<A::Error>,
{
    type Item = AndThen<FromErr<A::Service, T::Error>, T::Transform>;
    type Error = T::InitError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.fut_t.is_none() {
            if let Async::Ready(service) = self.fut_b.poll()? {
                self.fut_t = Some(self.t_cell.new_transform(service));
            }
        }

        if self.a.is_none() {
            if let Async::Ready(service) = self.fut_a.poll()? {
                self.a = Some(service);
            }
        }

        if let Some(ref mut fut) = self.fut_t {
            if let Async::Ready(transform) = fut.poll()? {
                self.t = Some(transform);
            }
        }

        if self.a.is_some() && self.t.is_some() {
            Ok(Async::Ready(AndThen::new(
                FromErr::new(self.a.take().unwrap()),
                self.t.take().unwrap(),
            )))
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
    impl Service<()> for Srv {
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

        let mut srv = blank
            .into_service()
            .apply_fn(Srv, |req: &'static str, srv: &mut Srv| {
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
