//! Utilities to provide interoperability between services based on the
//! `actix-service` and `tower-service` crates.
//!
//! ## Example
//!
//! In the following example, we take a `RandomService`—which will always
//! return 4—and wraps it with a middleware that will always add 1 to the
//! result. This pattern can be further used to wrap services from either
//! `tower-service` or `actix-service` with middleware provided by the other.
//!
//! ```
//! use actix_tower::ActixServiceExt;
//! # use futures::{Async, Future};
//! use actix_service::Service;
//!
//! struct RandomService;
//! impl Service for RandomService {
//!     // …
//! #    type Request = ();
//! #    type Response = u32;
//! #    type Error = ();
//! #    type Future = futures::future::FutureResult<Self::Response, Self::Error>;
//! #
//! #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
//! #        Ok(Async::Ready(()))
//! #    }
//! #
//! #    fn call(&mut self, _req: Self::Request) -> Self::Future {
//! #        futures::finished(4)
//! #     }
//! }
//!
//! struct AddOneMiddleware<S>(S);
//! impl<S, R> tower_service::Service<R> for AddOneMiddleware<S>
//! where
//!     S: tower_service::Service<R, Response = u32>,
//!     S::Future: 'static,
//! {
//!     /// …
//! #    type Response = u32;
//! #    type Error = S::Error;
//! #    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;
//! #
//! #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
//! #        self.0.poll_ready()
//! #    }
//! #
//! #    fn call(&mut self, req: R) -> Self::Future {
//! #        let fut = self.0.call(req).map(|x| x + 1);
//! #        Box::new(fut)
//! #    }
//! }
//!
//! let mut s = RandomService.wrap_with_tower_middleware(AddOneMiddleware);
//! assert_eq!(Ok(Async::Ready(())), s.poll_ready());
//! assert_eq!(Ok(Async::Ready(5)), s.call(()).poll());
//! ```

use actix_service::Service as ActixService;
use std::marker::PhantomData;
use tower_service::Service as TowerService;

/// Compatibility wrapper associating a `tower_service::Service` with a particular
/// `Request` type, so that it can be used as an `actix_service::Service`.
///
/// Generally created through convenience methods on the `TowerServiceExt<R>` trait.
pub struct ActixCompat<S, R> {
    inner: S,
    _phantom: PhantomData<R>,
}

impl<S, R> ActixCompat<S, R> {
    /// Wraps a `tower_service::Service` in a compatibility wrapper.
    pub fn new(inner: S) -> Self {
        ActixCompat {
            inner,
            _phantom: PhantomData,
        }
    }
}

/// Extension trait for wrapping a `tower_service::Service` instance for use as
/// an `actix_service::Service`.
pub trait TowerServiceExt<R> : TowerService<R> + Sized {
    /// Wraps a `tower_service::Service` in a compatibility wrapper.
    ///
    /// ```
    /// use actix_service::Service;
    /// use actix_tower::TowerServiceExt;
    /// # use futures::{Async, Future};
    ///
    /// struct RandomService;
    /// impl<R> tower_service::Service<R> for RandomService {
    ///     // …
    /// #    type Response = u32;
    /// #    type Error = ();
    /// #    type Future = futures::future::FutureResult<Self::Response, Self::Error>;
    /// #
    /// #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
    /// #        Ok(Async::Ready(()))
    /// #    }
    /// #
    /// #    fn call(&mut self, _req: R) -> Self::Future {
    /// #        futures::finished(4)
    /// #     }
    /// }
    ///
    /// let mut s = RandomService.into_actix_service();
    /// assert_eq!(Ok(Async::Ready(())), s.poll_ready());
    /// assert_eq!(Ok(Async::Ready(4)), s.call(()).poll());
    /// ```
    fn into_actix_service(self) -> ActixCompat<Self, R> {
        ActixCompat::new(self)
    }

    /// Takes a function that, when provided with an `actix_service::Service` wraps it
    /// and returns a new service. Useful for wrapping a `tower_service::Service` with
    /// middleware built for `actix_service`.
    ///
    /// ```
    /// use actix_tower::TowerServiceExt;
    /// # use futures::{Async, Future};
    /// use tower_service::Service;
    ///
    /// struct RandomService;
    /// impl<R> Service<R> for RandomService {
    ///     // …
    /// #    type Response = u32;
    /// #    type Error = ();
    /// #    type Future = futures::future::FutureResult<Self::Response, Self::Error>;
    /// #
    /// #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
    /// #        Ok(Async::Ready(()))
    /// #    }
    /// #
    /// #    fn call(&mut self, _req: R) -> Self::Future {
    /// #        futures::finished(4)
    /// #     }
    /// }
    ///
    /// struct AddOneTransform<S>(S);
    /// impl<S> actix_service::Service for AddOneTransform<S>
    /// where
    ///     S: actix_service::Service<Response = u32>,
    ///     S::Future: 'static,
    /// {
    ///     /// …
    /// #    type Request = S::Request;
    /// #    type Response = u32;
    /// #    type Error = S::Error;
    /// #    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;
    /// #
    /// #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
    /// #        self.0.poll_ready()
    /// #    }
    /// #
    /// #    fn call(&mut self, req: Self::Request) -> Self::Future {
    /// #        let fut = self.0.call(req).map(|x| x + 1);
    /// #        Box::new(fut)
    /// #    }
    /// }
    ///
    /// let mut s = RandomService.wrap_with_actix_middleware(AddOneTransform);
    /// assert_eq!(Ok(Async::Ready(())), s.poll_ready());
    /// assert_eq!(Ok(Async::Ready(5)), s.call(()).poll());
    /// ```
    fn wrap_with_actix_middleware<F, U>(self, f: F) -> TowerCompat<U>
    where
<<<<<<< HEAD
        Self: TowerService<R> + Sized,
=======
        F: FnOnce(ActixCompat<Self, R>) -> U,
        U: ActixService<Request = R>
>>>>>>> 14e3933d8b4c68859fb0bea9d99d712280e0cce4
    {
        f(self.into_actix_service()).into_tower_service()
    }
}

impl<S, R> TowerServiceExt<R> for S
where
    S: TowerService<R> + Sized
{}

impl<S, R> ActixService for ActixCompat<S, R>
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

/// Compatibility wrapper associating an `actix_service::Service` with a particular
/// `Request` type, so that it can be used as a `tower_service::Service`.
///
/// Generally created through convenience methods on the `ActixServiceExt<R>` trait.
pub struct TowerCompat<S> {
    inner: S,
}

impl<S> TowerCompat<S> {
    /// Wraps an `actix_service::Service` in a compatibility wrapper.
    pub fn new(inner: S) -> Self {
        TowerCompat {
            inner,
        }
    }
}

/// Extension trait for wrapping an `actix_service::Service` instance for use as
/// a `tower_service::Service`.
pub trait ActixServiceExt: ActixService + Sized {
    /// Wraps a `tower_service::Service` in a compatibility wrapper.
    ///
    /// ```
    /// use actix_tower::ActixServiceExt;
    /// # use futures::{Async, Future};
    /// use tower_service::Service;
    ///
    /// struct RandomService;
    /// impl actix_service::Service for RandomService {
    ///     // …
    /// #    type Request = ();
    /// #    type Response = u32;
    /// #    type Error = ();
    /// #    type Future = futures::future::FutureResult<Self::Response, Self::Error>;
    /// #
    /// #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
    /// #        Ok(Async::Ready(()))
    /// #    }
    /// #
    /// #    fn call(&mut self, _req: Self::Request) -> Self::Future {
    /// #        futures::finished(4)
    /// #     }
    /// }
    ///
    /// let mut s = RandomService.into_tower_service();
    /// assert_eq!(Ok(Async::Ready(())), s.poll_ready());
    /// assert_eq!(Ok(Async::Ready(4)), s.call(()).poll());
    /// ```
    fn into_tower_service(self) -> TowerCompat<Self> {
        TowerCompat::new(self)
    }

    /// Takes a function that, when provided with a `tower_service::Service` wraps it
    /// and returns a new service. Useful for wrapping an `actix_service::Service` with
    /// middleware built for `tower_service`.
    ///
    /// ```
    /// use actix_tower::ActixServiceExt;
    /// # use futures::{Async, Future};
    /// use actix_service::Service;
    ///
    /// struct RandomService;
    /// impl Service for RandomService {
    ///     // …
    /// #    type Request = ();
    /// #    type Response = u32;
    /// #    type Error = ();
    /// #    type Future = futures::future::FutureResult<Self::Response, Self::Error>;
    /// #
    /// #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
    /// #        Ok(Async::Ready(()))
    /// #    }
    /// #
    /// #    fn call(&mut self, _req: Self::Request) -> Self::Future {
    /// #        futures::finished(4)
    /// #     }
    /// }
    ///
    /// struct AddOneMiddleware<S>(S);
    /// impl<S, R> tower_service::Service<R> for AddOneMiddleware<S>
    /// where
    ///     S: tower_service::Service<R, Response = u32>,
    ///     S::Future: 'static,
    /// {
    ///     /// …
    /// #    type Response = u32;
    /// #    type Error = S::Error;
    /// #    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;
    /// #
    /// #    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
    /// #        self.0.poll_ready()
    /// #    }
    /// #
    /// #    fn call(&mut self, req: R) -> Self::Future {
    /// #        let fut = self.0.call(req).map(|x| x + 1);
    /// #        Box::new(fut)
    /// #    }
    /// }
    ///
    /// let mut s = RandomService.wrap_with_tower_middleware(AddOneMiddleware);
    /// assert_eq!(Ok(Async::Ready(())), s.poll_ready());
    /// assert_eq!(Ok(Async::Ready(5)), s.call(()).poll());
    /// ```
    fn wrap_with_tower_middleware<F, U>(self, f: F) -> ActixCompat<U, Self::Request>
    where
        F: FnOnce(TowerCompat<Self>) -> U,
        U: TowerService<Self::Request>
    {
        f(self.into_tower_service()).into_actix_service()
    }
}

impl<S> ActixServiceExt for S
where
    S: ActixService + Sized
{}

impl<S> TowerService<S::Request> for TowerCompat<S>
where
    S: ActixService,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> futures::Poll<(), Self::Error> {
        ActixService::poll_ready(&mut self.inner)
    }

    fn call(&mut self, req: S::Request) -> Self::Future {
        ActixService::call(&mut self.inner, req)
    }
}

#[cfg(test)]
mod tests {
<<<<<<< HEAD
    use super::TowerServiceExt;
    use actix_service::{Service as ActixService, ServiceExt, Transform};
    use futures::{future::FutureResult, Async, Future, Poll};
    use tower_service::Service as TowerService;
=======
    mod tower_service_into_actix_service {
        use crate::TowerServiceExt;
        use actix_service::{Service as ActixService, ServiceExt, Transform};
        use futures::{future::FutureResult, Async, Poll, Future};
        use tower_service::Service as TowerService;


        #[test]
        fn random_service_returns_4() {
            let mut s = RandomService.into_actix_service();

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());
>>>>>>> 14e3933d8b4c68859fb0bea9d99d712280e0cce4

            assert_eq!(Ok(Async::Ready(4)), s.call(()).poll());
        }

        #[test]
        fn random_service_can_combine() {
            let mut s = RandomService.into_actix_service().map(|x| x + 1);

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(5)), s.call(()).poll());
        }

        #[test]
        fn random_service_can_use_actix_middleware() {
            let mut s = RandomService.wrap_with_actix_middleware(DoMathTransform);

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(68)), s.call(()).poll());
        }

        #[test]
        fn random_service_and_add_service_chained() {
            let s1 = RandomService.into_actix_service();
            let s2 = AddOneService.into_actix_service();
            let s3 = AddOneService.into_actix_service();

            let mut s = s1.and_then(s2).and_then(s3);

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(6)), s.call(()).poll());
        }

        #[test]
        fn random_service_and_add_service_and_ignoring_service_chained() {
            let s1 = RandomService.into_actix_service();
            let s2 = AddOneService.into_actix_service();
            let s3 = AddOneService.into_actix_service();
            let s4 = RandomService.into_actix_service();

            let mut s = s1.and_then(s2).and_then(s3).and_then(s4);

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(4)), s.call(()).poll());
        }

        #[test]
        fn random_service_can_be_transformed_to_do_math() {
            let transform = DoMath;

            let mut s = transform.new_transform(RandomService.into_actix_service()).wait().unwrap();

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(68)), s.call(()).poll());
        }

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
    }

    mod actix_service_into_tower_service {
        use crate::{ActixServiceExt, TowerServiceExt};
        use actix_service::{Service as ActixService, ServiceExt};
        use futures::{future::FutureResult, Async, Poll, Future};
        use tower_service::Service as TowerService;


        #[test]
        fn random_service_returns_4() {
            let mut s = RandomService.into_tower_service();

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(4)), s.call(()).poll());
        }

        #[test]
        fn random_service_can_use_tower_middleware() {
            let mut s = AddOneService::wrap(RandomService.into_tower_service()).into_actix_service();

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(5)), s.call(()).poll());
        }

        #[test]
        fn do_math_service_can_use_tower_middleware() {
            let mut s = AddOneService::wrap(DoMathService.into_tower_service()).into_actix_service();

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

            assert_eq!(Ok(Async::Ready(188)), s.call(11).poll());
        }

        #[test]
        fn random_service_and_add_service_and_ignoring_service_chained() {
            let s1 = RandomService.wrap_with_tower_middleware(AddOneService::wrap);
            let s2 = DoMathService.wrap_with_tower_middleware(AddOneService::wrap);

<<<<<<< HEAD
        let mut s = transform
            .new_transform(RandomService.compat())
            .wait()
            .unwrap();
=======
            let mut s = s1.and_then(s2);
>>>>>>> 14e3933d8b4c68859fb0bea9d99d712280e0cce4

            assert_eq!(Ok(Async::Ready(())), s.poll_ready());

<<<<<<< HEAD
        assert_eq!(Ok(Async::Ready(68)), s.call(()).poll());
    }
}
=======
            assert_eq!(Ok(Async::Ready(86)), s.call(()).poll());
        }

        struct RandomService;
        impl ActixService for RandomService {
            type Request = ();
            type Response = u32;
            type Error = ();
            type Future = FutureResult<Self::Response, Self::Error>;

            fn poll_ready(&mut self) -> Poll<(), Self::Error> {
                Ok(Async::Ready(()))
            }

            fn call(&mut self, _req: Self::Request) -> Self::Future {
                futures::finished(4)
            }
        }

        struct AddOneService<S> {
            inner: S
        }

        impl<S> AddOneService<S> {
            fn wrap(inner: S) -> Self {
                AddOneService {
                    inner,
                }
            }
        }

        impl<S, R> TowerService<R> for AddOneService<S>
        where
            S: TowerService<R, Response = u32>,
            S::Future: 'static,
        {
            type Response = u32;
            type Error = S::Error;
            type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error>>;

            fn poll_ready(&mut self) -> Poll<(), Self::Error> {
                self.inner.poll_ready()
            }

            fn call(&mut self, req: R) -> Self::Future {
                let fut = self.inner.call(req)
                    .map(|x| x + 1);

                Box::new(fut)
            }
        }

        struct DoMathService;
        impl ActixService for DoMathService {
            type Request = u32;
            type Response = u32;
            type Error = ();
            type Future = FutureResult<Self::Response, Self::Error>;

            fn poll_ready(&mut self) -> Poll<(), Self::Error> {
                Ok(Async::Ready(()))
            }

            fn call(&mut self, req: Self::Request) -> Self::Future {
                futures::finished(req * 17)
            }
        }}
}
>>>>>>> 14e3933d8b4c68859fb0bea9d99d712280e0cce4
