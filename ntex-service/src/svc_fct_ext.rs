use crate::{IntoServiceFactory, Middleware, Pipeline, ServiceFactory, boxed, dev::{self, AndThenFactory, ApplyCtx, ApplyFactory, ApplyMiddleware, InspectErrFactory, ThenFactory}, inspect::InspectFactory};

pub trait ServiceFactoryExt<Req, Cfg = ()>: ServiceFactory<Req, Cfg> {
    #[inline]
    /// Asynchronously creates a new service and wraps it in a container.
    async fn pipeline(&self, cfg: Cfg) -> Result<Pipeline<Self::Service>, Self::InitError>
    where
        Self: Sized,
    {
        Ok(Pipeline::new(self.create(cfg).await?))
    }

    #[inline]
    /// Returns a new service that maps this service's output to a different type.
    fn map<F, Res>(
        self,
        f: F,
    ) -> dev::MapFactory<Self, F, Req, Res, Cfg>
    where
        Self: Sized,
        F: Fn(Self::Response) -> Res + Clone,
    {
        dev::MapFactory::new(self, f)
    }

    #[inline]
    /// Transforms this service's error into another error,
    /// producing a new service.
    fn map_err<F, E>(
        self,
        f: F,
    ) -> dev::MapErrFactory<Self, Req, Cfg, F, E>
    where
        Self: Sized,
        F: Fn(Self::Error) -> E + Clone,
    {
        dev::MapErrFactory::new(self, f)
    }

    #[inline]
    /// Maps this factory's initialization error to a different error,
    /// returning a new service factory.
    fn map_init_err<F, E>(
        self,
        f: F,
    ) -> dev::MapInitErr<Self, Req, Cfg, F, E>
    where
        Self: Sized,
        F: Fn(Self::InitError) -> E + Clone,
    {
        dev::MapInitErr::new(self, f)
    }

    /// Creates a boxed service factory.
    fn boxed(
        self,
    ) -> boxed::BoxServiceFactory<Cfg, Req, Self::Response, Self::Error, Self::InitError>
    where
        Self: 'static + Sized,
        Cfg: 'static,
        Req: 'static,
    {
        boxed::factory(self)
    }
    
    fn and_then<F, U>(
        self,
        factory: F,
    ) -> AndThenFactory<Self, U>
    where
        Self: Sized,
        F: IntoServiceFactory<U, Self::Response, Cfg>,
        U: ServiceFactory<Self::Response, Cfg, Error = Self::Error, InitError = Self::InitError>,
    {
        AndThenFactory::new(self, factory.into_factory())
    }

    /// Apply Middleware to current service factory.
    ///
    /// Short version of `apply(middleware, chain_factory(...))`
    fn apply<U>(self, tr: U) -> ApplyMiddleware<U, Self, Cfg>
    where
        Self: Sized,
        U: Middleware<Self::Service, Cfg>,
    {
        crate::apply(tr, self)
    }

    /// Apply function middleware to current service factory.
    ///
    /// Short version of `apply_fn_factory(chain_factory(...), fn)`
    fn apply_fn<F, In, Out, Err>(
        self,
        f: F,
    ) -> ApplyFactory<Self, Req, Cfg, F, In, Out, Err>
    where
        Self: Sized + ServiceFactory<Req, Cfg>,
        F: AsyncFn(In, &ApplyCtx<'_, Self::Service>) -> Result<Out, Err> + Clone,
        Err: From<Self::Error>,
    {
        crate::apply_fn_factory(self, f)
    }

    /// Create chain factory to chain on a computation for when a call to the
    /// service finished, passing the result of the call to the next
    /// service `U`.
    ///
    /// Note that this function consumes the receiving factory and returns a
    /// wrapped version of it.
    fn then<F, U>(self, factory: F) -> ThenFactory<Self, U>
    where
        Self: Sized,
        Cfg: Clone,
        F: IntoServiceFactory<U, Result<Self::Response, Self::Error>, Cfg>,
        U: ServiceFactory<
                Result<Self::Response, Self::Error>,
                Cfg,
                Error = Self::Error,
                InitError = Self::InitError,
            >,
    {
        ThenFactory::new(self, factory.into_factory())
    }

    /// Calls a function with a reference to the contained value if Ok.
    ///
    /// Returns the original result.
    fn inspect<F>(self, f: F) -> InspectFactory<Self, F>
    where
        Self: Sized,
        F: Fn(&Self::Response) + Clone,
    {
        InspectFactory::new(self, f)
    }

    /// Calls a function with a reference to the contained value if Err.
    ///
    /// Returns the original result.
    fn inspect_err<F>(
        self,
        f: F,
    ) -> InspectErrFactory<Self, F>
    where
        Self: Sized,
        F: Fn(&Self::Error) + Clone,
    {
        InspectErrFactory::new(self, f)
    }
}

impl<S, Req, Cfg> ServiceFactoryExt<Req, Cfg> for S
where S: ServiceFactory<Req, Cfg> {}