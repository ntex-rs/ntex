use std::marker::PhantomData;

use crate::{IntoNewService, NewService};

/// Create new ApplyConfig` service factory combinator
pub fn apply_cfg<F, S, C1, C2, U>(f: F, service: U) -> ApplyConfig<F, S, C1, C2>
where
    S: NewService<C2>,
    F: Fn(&C1) -> C2,
    U: IntoNewService<S, C2>,
{
    ApplyConfig::new(service.into_new_service(), f)
}

/// `ApplyConfig` service factory combinator
pub struct ApplyConfig<F, S, C1, C2> {
    s: S,
    f: F,
    r: PhantomData<(C1, C2)>,
}

impl<F, S, C1, C2> ApplyConfig<F, S, C1, C2>
where
    S: NewService<C2>,
    F: Fn(&C1) -> C2,
{
    /// Create new ApplyConfig` service factory combinator
    pub fn new<U: IntoNewService<S, C2>>(a: U, f: F) -> Self {
        Self {
            f,
            s: a.into_new_service(),
            r: PhantomData,
        }
    }
}

impl<F, S, C1, C2> Clone for ApplyConfig<F, S, C1, C2>
where
    S: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            s: self.s.clone(),
            f: self.f.clone(),
            r: PhantomData,
        }
    }
}

impl<F, S, C1, C2> NewService<C1> for ApplyConfig<F, S, C1, C2>
where
    S: NewService<C2>,
    F: Fn(&C1) -> C2,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Service = S::Service;

    type InitError = S::InitError;
    type Future = S::Future;

    fn new_service(&self, cfg: &C1) -> Self::Future {
        let cfg2 = (self.f)(cfg);

        self.s.new_service(&cfg2)
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
