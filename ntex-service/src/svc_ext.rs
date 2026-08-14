use crate::{IntoService, Pipeline, Service, dev::{self, AndThen, Apply, ApplyCtx, InspectErr, Then}, inspect::Inspect};

pub trait ServiceExt<Req>: Service<Req> {
    #[inline]
    /// Maps this service's output to a different type, returning a new service.
    ///
    /// This is similar to `Option::map` or `Iterator::map`, changing the
    /// output type of the underlying service.
    ///
    /// This function consumes the original service and returns a wrapped version,
    /// following the pattern of standard library `map` methods.
    fn map<F, Res>(self, f: F) -> dev::Map<Self, F, Req, Res>
    where
        Self: Sized,
        F: Fn(Self::Response) -> Res,
    {
        dev::Map::new(self, f)
    }

    #[inline]
    /// Maps this service's error to a different type, returning a new service.
    ///
    /// This is similar to `Result::map_err`, changing the error type of the
    /// underlying service. It is useful, for example, to ensure multiple
    /// services have the same error type.
    ///
    /// This function consumes the original service and returns a wrapped version.
    fn map_err<F, E>(self, f: F) -> dev::MapErr<Self, F, E>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E,
    {
        dev::MapErr::new(self, f)
    }

    fn and_then<Next, F>(self, service: F) -> AndThen<Self, Next>
    where
        Self: Sized,
        F: IntoService<Next, Self::Response>,
        Next: Service<Self::Response, Error = Self::Error>,
    {
        AndThen::new(self, service.into_service())
    }

    /// Chain on a computation for when a call to the service finished,
    /// passing the result of the call to the next service `U`.
    fn then<Next, F>(self, service: F) -> Then<Self, Next>
    where
        Self: Sized,
        F: IntoService<Next, Result<Self::Response, Self::Error>>,
        Next: Service<Result<Self::Response, Self::Error>, Error = Self::Error>,
    {
        Then::new(self, service.into_service())
    }

    /// Calls a function with a reference to the contained value if Ok.
    ///
    /// Returns the original result.
    fn inspect<F>(self, f: F) -> Inspect<Self, F>
    where
        Self: Sized,
        F: Fn(&Self::Response),
    {
        Inspect::new(self, f)
    }

    /// Calls a function with a reference to the contained value if Err.
    ///
    /// Returns the original result.
    fn inspect_err<F>(self, f: F) -> InspectErr<Self, F>
    where
        Self: Sized,
        F: Fn(&Self::Error),
    {
        InspectErr::new(self, f)
    }

    /// Use function as middleware for current service.
    ///
    /// Short version of `apply_fn(chain(...), fn)`
    fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> Apply<Self, Req, F, In, Out, Err>
    where
        Self: Sized + Service<Req>,
        F: AsyncFn(In, &ApplyCtx<'_, Self>) -> Result<Out, Err>,
        Err: From<Self::Error>,
    {
        crate::apply_fn(self, f)
    }

    /// Create service pipeline
    fn into_pipeline(self) -> Pipeline<Self>
    where 
        Self: Sized {
        Pipeline::new(self)
    }
}

impl<S, Req> ServiceExt<Req> for S
where S: Service<Req> {}