use std::{convert::Infallible, fmt, marker::PhantomData};

use crate::{Ctx, Service, ServiceFactory};

/// A pass-through service with readiness controlled by an asynchronous function.
///
/// The callback receives only pipeline state, not a readiness scope. For scoped
/// readiness and change notifications, implement [`Service`] directly.
pub struct FnReadiness<F, Err> {
    f: F,
    err: PhantomData<Err>,
}

impl<F, Err> FnReadiness<F, Err> {
    /// Creates a readiness service from an asynchronous function.
    pub fn new<St>(f: F) -> Self
    where
        F: AsyncFn(&St) -> Result<(), Err>,
    {
        Self {
            f,
            err: PhantomData,
        }
    }
}

impl<F, Err> Clone for FnReadiness<F, Err>
where
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        Self {
            f: self.f.clone(),
            err: PhantomData,
        }
    }
}

impl<F, Err> fmt::Debug for FnReadiness<F, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnReadiness")
            .field("fn", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, Err, St, Req> ServiceFactory<St, Req> for FnReadiness<F, Err>
where
    F: AsyncFn(&St) -> Result<(), Err> + Clone,
{
    type Res = Req;
    type Error = Err;

    type Service = FnReadiness<F, Err>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        Ok(self.clone())
    }
}

impl<F, St, Req, Err> Service<St, Req> for FnReadiness<F, Err>
where
    F: AsyncFn(&St) -> Result<(), Err> + Clone,
{
    type Res = Req;
    type Error = Err;

    #[inline]
    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Err> {
        (self.f)(ctx.st()).await
    }

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Req, Err> {
        Ok(req)
    }
}

#[cfg(test)]
#[path = "../tests/unit/fn_ready.rs"]
mod tests;
