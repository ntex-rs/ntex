use std::marker::PhantomData;

use futures::{Async, Future, Poll};

use crate::and_then::AndThen;
use crate::{IntoNewService, NewService};

/// `ApplyNewService` new service combinator
pub struct ApplyConfig<F, A, B, C1, C2> {
    a: A,
    b: B,
    f: F,
    r: PhantomData<(C1, C2)>,
}

impl<F, A, B, C1, C2> ApplyConfig<F, A, B, C1, C2>
where
    A: NewService<C1>,
    B: NewService<C2, Request = A::Response, Error = A::Error, InitError = A::InitError>,
    F: Fn(&C1) -> C2,
{
    /// Create new `ApplyNewService` new service instance
    pub fn new<A1: IntoNewService<A, C1>, B1: IntoNewService<B, C2>>(
        a: A1,
        b: B1,
        f: F,
    ) -> Self {
        Self {
            f,
            a: a.into_new_service(),
            b: b.into_new_service(),
            r: PhantomData,
        }
    }
}

impl<F, A, B, C1, C2> Clone for ApplyConfig<F, A, B, C1, C2>
where
    A: Clone,
    B: Clone,
    F: Clone,
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

impl<F, A, B, C1, C2> NewService<C1> for ApplyConfig<F, A, B, C1, C2>
where
    A: NewService<C1>,
    B: NewService<C2, Request = A::Response, Error = A::Error, InitError = A::InitError>,
    F: Fn(&C1) -> C2,
{
    type Request = A::Request;
    type Response = B::Response;
    type Error = A::Error;
    type Service = AndThen<A::Service, B::Service>;

    type InitError = A::InitError;
    type Future = ApplyConfigResponse<A, B, C1, C2>;

    fn new_service(&self, cfg: &C1) -> Self::Future {
        let cfg2 = (self.f)(cfg);

        ApplyConfigResponse {
            a: None,
            b: None,
            fut_a: self.a.new_service(cfg),
            fut_b: self.b.new_service(&cfg2),
        }
    }
}

pub struct ApplyConfigResponse<A, B, C1, C2>
where
    A: NewService<C1>,
    B: NewService<C2>,
{
    fut_b: B::Future,
    fut_a: A::Future,
    a: Option<A::Service>,
    b: Option<B::Service>,
}

impl<A, B, C1, C2> Future for ApplyConfigResponse<A, B, C1, C2>
where
    A: NewService<C1>,
    B: NewService<C2, Request = A::Response, Error = A::Error, InitError = A::InitError>,
{
    type Item = AndThen<A::Service, B::Service>;
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
            Ok(Async::Ready(AndThen::new(
                self.a.take().unwrap(),
                self.b.take().unwrap(),
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

    use crate::{fn_cfg_factory, NewService, Service};

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
    fn test_new_service() {
        let new_srv = fn_cfg_factory(|_: &usize| Ok::<_, ()>(Srv)).apply_cfg(
            fn_cfg_factory(|s: &String| {
                assert_eq!(s, "test");
                Ok::<_, ()>(Srv)
            }),
            |cfg: &usize| {
                assert_eq!(*cfg, 1);
                "test".to_string()
            },
        );

        if let Async::Ready(mut srv) = new_srv.new_service(&1).poll().unwrap() {
            assert!(srv.poll_ready().is_ok());
        }
    }
}
