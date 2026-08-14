use crate::{Service, chain, dev};

pub trait ServiceExt<Req>: Service<Req> {
    #[inline]
    /// Maps this service's output to a different type, returning a new service.
    ///
    /// This is similar to `Option::map` or `Iterator::map`, changing the
    /// output type of the underlying service.
    ///
    /// This function consumes the original service and returns a wrapped version,
    /// following the pattern of standard library `map` methods.
    fn map<F, Res>(self, f: F) -> dev::ServiceChain<dev::Map<Self, F, Req, Res>, Req>
    where
        Self: Sized,
        F: Fn(Self::Response) -> Res,
    {
        chain(dev::Map::new(self, f))
    }

    #[inline]
    /// Maps this service's error to a different type, returning a new service.
    ///
    /// This is similar to `Result::map_err`, changing the error type of the
    /// underlying service. It is useful, for example, to ensure multiple
    /// services have the same error type.
    ///
    /// This function consumes the original service and returns a wrapped version.
    fn map_err<F, E>(self, f: F) -> dev::ServiceChain<dev::MapErr<Self, F, E>, Req>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        chain(dev::MapErr::new(self, f))
    }
}
