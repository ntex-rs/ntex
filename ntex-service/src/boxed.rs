//! Type-erased services and service factories.
//!
//! Use [`service()`] or [`BoxService::new`] to hide a service's concrete type
//! while keeping its state, request, response, and error types explicit.
//! This lets different implementations share a return type or collection.
//! [`factory()`] and [`BoxServiceFactory`] do the same for factories; each
//! created service is returned as a [`BoxService`].
//!
//! These wrappers use reference-counted, local handles. Cloning shares the
//! underlying service or factory rather than cloning its implementation.
//! Binding, readiness, calls, and shutdown are forwarded to the wrapped
//! service. Type erasure preserves existing readiness caches but does not add
//! one to a raw service; use [`crate::Pipeline`] or [`crate::ready()`] when a
//! readiness cache is needed.
//!
//! The wrapped service's `ready`, `call`, and `shutdown` futures are stored in
//! an [`InlineFuture`] slot of `WORDS` machine words. Futures that fit don't
//! allocate, larger futures are boxed. The default `WORDS = 0` boxes every
//! future larger than a pointer. Use [`BoxService::new_inline`] or
//! [`BoxServiceFactory::new_inline`] to pick a larger slot.
//!
//! # Example
//!
//! ```
//! use ntex_service::{boxed, fn_service};
//!
//! fn handler(double: bool) -> boxed::BoxService<(), u32, u32, ()> {
//!     if double {
//!         boxed::service(fn_service(async |req: u32| Ok::<_, ()>(req * 2)))
//!     } else {
//!         boxed::service(fn_service(async |req: u32| Ok::<_, ()>(req)))
//!     }
//! }
//! ```
//!
//! # Framework guide
//!
//! - [Service Model](https://docs.ntex.rs/1-service.html)
//! - [Service Pipelines](https://docs.ntex.rs/2-pipeline.html)

use std::{fmt, rc::Rc};

use crate::{BindCtx, Ctx, inline::InlineFuture};
use crate::{Service, ServiceFactory, util::BoxFuture};

// ============================ Service =============================

/// Boxed service.
///
/// Type erasure preserves the wrapped service's readiness identity. Clones
/// share that service and any bound readiness; boxing a raw service does not
/// give it a cache.
///
/// The wrapped service's futures are stored inline when they fit into `WORDS`
/// machine words and boxed otherwise, see [`InlineFuture`]. Boxed services
/// with different `WORDS` are different types.
pub struct BoxService<St, Req, Res, Err, const WORDS: usize = 0> {
    inner: Rc<dyn ServiceObj<St, Req, WORDS, Res = Res, Error = Err>>,
}

/// Creates a boxed service.
pub fn service<S, St, Req, Res, Err>(service: S) -> BoxService<St, Req, Res, Err>
where
    S: Service<St, Req, Res = Res, Error = Err> + 'static,
{
    BoxService::new(service)
}

impl<St, Req, Res, Err> BoxService<St, Req, Res, Err> {
    /// Creates a boxed service with the default slot (`WORDS = 0`).
    pub fn new<S>(service: S) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
    {
        Self::new_inline(service)
    }
}

impl<St, Req, Res, Err, const WORDS: usize> BoxService<St, Req, Res, Err, WORDS> {
    /// Creates a boxed service that stores futures of up to `WORDS` machine
    /// words inline.
    ///
    /// `WORDS` is usually inferred from the annotated type:
    ///
    /// ```
    /// use ntex_service::{boxed::BoxService, fn_service};
    ///
    /// let svc: BoxService<(), u32, u32, (), 4> =
    ///     BoxService::new_inline(fn_service(async |req: u32| Ok::<_, ()>(req)));
    /// ```
    pub fn new_inline<S>(service: S) -> Self
    where
        S: Service<St, Req, Res = Res, Error = Err> + 'static,
    {
        BoxService {
            inner: Rc::new(service),
        }
    }
}

impl<St, Req, Res, Err, const WORDS: usize> Clone for BoxService<St, Req, Res, Err, WORDS> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<St, Req, Res, Err, const WORDS: usize> fmt::Debug for BoxService<St, Req, Res, Err, WORDS> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxService").finish()
    }
}

impl<St, Req, Res, Err, const WORDS: usize> Service<St, Req>
    for BoxService<St, Req, Res, Err, WORDS>
{
    type Res = Res;
    type Error = Err;

    fn bind(&self, ctx: BindCtx<'_>) {
        self.inner.bind(ctx);
    }

    #[inline]
    #[allow(clippy::manual_async_fn)]
    fn ready(&self, ctx: Ctx<'_, Self, St>) -> impl Future<Output = Result<(), Self::Error>> {
        // An `async fn` would store `ctx` twice. The block is still needed
        // because the inner future borrows both `self` and `ctx` under one lifetime.
        async move { self.inner.ready(ctx.cast()).await }
    }

    #[inline]
    #[allow(clippy::manual_async_fn)]
    fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> impl Future<Output = Result<Res, Err>> {
        // See `ready`.
        async move { self.inner.call(req, ctx.cast()).await }
    }

    #[inline]
    #[allow(clippy::manual_async_fn)]
    fn shutdown(&self, ctx: Ctx<'_, Self, St>) -> impl Future<Output = ()> {
        // See `ready`.
        async move { self.inner.shutdown(ctx.cast()).await }
    }
}

trait ServiceObj<St, Req, const WORDS: usize> {
    type Res;
    type Error;

    fn bind(&self, ctx: BindCtx<'_>);

    fn ready<'a>(
        &'a self,
        ctx: Ctx<'a, (), St>,
    ) -> InlineFuture<'a, Result<(), Self::Error>, WORDS>
    where
        Req: 'a;

    fn call<'a>(
        &'a self,
        r: Req,
        ctx: Ctx<'a, (), St>,
    ) -> InlineFuture<'a, Result<Self::Res, Self::Error>, WORDS>
    where
        Req: 'a;

    fn shutdown<'a>(&'a self, ctx: Ctx<'a, (), St>) -> InlineFuture<'a, (), WORDS>
    where
        St: 'a,
        Req: 'a;
}

impl<S, St, Req, const WORDS: usize> ServiceObj<St, Req, WORDS> for S
where
    S: Service<St, Req>,
{
    type Res = S::Res;
    type Error = S::Error;

    fn bind(&self, ctx: BindCtx<'_>) {
        Service::bind(self, ctx);
    }

    #[inline]
    fn ready<'a>(&'a self, ctx: Ctx<'a, (), St>) -> InlineFuture<'a, Result<(), Self::Error>, WORDS>
    where
        Req: 'a,
    {
        InlineFuture::new_with(|| Service::ready(self, ctx.cast()))
    }

    #[inline]
    fn call<'a>(
        &'a self,
        req: Req,
        ctx: Ctx<'a, (), St>,
    ) -> InlineFuture<'a, Result<S::Res, S::Error>, WORDS>
    where
        Req: 'a,
    {
        InlineFuture::new_with(|| Service::call(self, req, ctx.cast()))
    }

    #[inline]
    fn shutdown<'a>(&'a self, ctx: Ctx<'a, (), St>) -> InlineFuture<'a, (), WORDS>
    where
        St: 'a,
        Req: 'a,
    {
        InlineFuture::new_with(|| Service::shutdown(self, ctx.cast()))
    }
}

// ============================ ServiceFactory =============================

/// Boxed service factory.
///
/// Created services are [`BoxService`]s with the same `WORDS` slot size.
pub struct BoxServiceFactory<St, Req, Res, Err, InitErr, const WORDS: usize = 0> {
    inner: Rc<dyn ServiceFactoryObj<St, Req, WORDS, Res = Res, Error = Err, InitErr = InitErr>>,
}

/// Creates a boxed service factory.
pub fn factory<Sf, St, Req>(
    factory: Sf,
) -> BoxServiceFactory<St, Req, Sf::Res, Sf::Error, Sf::InitError>
where
    Sf: ServiceFactory<St, Req> + 'static,
    St: 'static,
    Req: 'static,
{
    BoxServiceFactory::new(factory)
}

impl<St, Req, Res, Err, InitErr> BoxServiceFactory<St, Req, Res, Err, InitErr>
where
    St: 'static,
    Req: 'static,
{
    /// Creates a boxed service factory with the default slot (`WORDS = 0`).
    pub fn new<Sf>(factory: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Res = Res, Error = Err, InitError = InitErr> + 'static,
    {
        Self::new_inline(factory)
    }
}

impl<St, Req, Res, Err, InitErr, const WORDS: usize>
    BoxServiceFactory<St, Req, Res, Err, InitErr, WORDS>
where
    St: 'static,
    Req: 'static,
{
    /// Creates a boxed service factory whose services store futures of up to
    /// `WORDS` machine words inline, see [`BoxService::new_inline`].
    pub fn new_inline<Sf>(factory: Sf) -> Self
    where
        Sf: ServiceFactory<St, Req, Res = Res, Error = Err, InitError = InitErr> + 'static,
    {
        Self {
            inner: Rc::new(factory),
        }
    }
}

impl<St, Req, Res, Err, InitErr, const WORDS: usize> ServiceFactory<St, Req>
    for BoxServiceFactory<St, Req, Res, Err, InitErr, WORDS>
where
    Req: 'static,
{
    type Res = Res;
    type Error = Err;

    type Service = BoxService<St, Req, Res, Err, WORDS>;
    type InitError = InitErr;

    #[inline]
    async fn create(&self, st: &St) -> Result<Self::Service, Self::InitError> {
        self.inner.create(st).await
    }
}

impl<St, Req, Res, Err, InitErr, const WORDS: usize> Clone
    for BoxServiceFactory<St, Req, Res, Err, InitErr, WORDS>
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<St, Req, Res, Err, InitErr, const WORDS: usize> fmt::Debug
    for BoxServiceFactory<St, Req, Res, Err, InitErr, WORDS>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxServiceFactory").finish()
    }
}

trait ServiceFactoryObj<St, Req, const WORDS: usize> {
    type Res;
    type Error;
    type InitErr;

    fn create<'a>(
        &'a self,
        st: &'a St,
    ) -> BoxFuture<'a, Result<BoxService<St, Req, Self::Res, Self::Error, WORDS>, Self::InitErr>>
    where
        Req: 'a;
}

impl<Sf, St, Req, const WORDS: usize> ServiceFactoryObj<St, Req, WORDS> for Sf
where
    St: 'static,
    Req: 'static,
    Sf: ServiceFactory<St, Req> + 'static,
{
    type Res = Sf::Res;
    type Error = Sf::Error;
    type InitErr = Sf::InitError;

    #[inline]
    fn create<'a>(
        &'a self,
        st: &'a St,
    ) -> BoxFuture<'a, Result<BoxService<St, Req, Self::Res, Self::Error, WORDS>, Self::InitErr>>
    where
        Req: 'a,
    {
        let fut = ServiceFactory::create(self, st);
        Box::pin(async move { fut.await.map(BoxService::new_inline) })
    }
}
