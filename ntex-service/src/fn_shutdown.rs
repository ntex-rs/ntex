use std::{cell::Cell, convert::Infallible, fmt, marker::PhantomData};

use crate::{Ctx, Service, ServiceFactory};

/// A pass-through service that invokes an asynchronous shutdown callback once.
pub struct FnShutdown<F, Err> {
    f_shutdown: Cell<Option<F>>,
    err: PhantomData<Err>,
}

impl<F, Err> FnShutdown<F, Err> {
    /// Creates a service with the supplied shutdown callback.
    pub fn new<St>(f: F) -> Self
    where
        F: AsyncFnOnce(&St),
    {
        Self {
            f_shutdown: Cell::new(Some(f)),
            err: PhantomData,
        }
    }
}

impl<F, Err> Clone for FnShutdown<F, Err>
where
    F: Clone,
{
    #[inline]
    fn clone(&self) -> Self {
        let f = self.f_shutdown.take();
        self.f_shutdown.set(f.clone());
        Self {
            f_shutdown: Cell::new(f),
            err: PhantomData,
        }
    }
}

impl<F, Err> fmt::Debug for FnShutdown<F, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnShutdown")
            .field("fn", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, St, Req, Err> Service<St, Req> for FnShutdown<F, Err>
where
    F: AsyncFnOnce(&St),
{
    type Res = Req;
    type Error = Err;

    #[inline]
    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        if let Some(f) = self.f_shutdown.take() {
            (f)(ctx.st()).await;
        }
    }

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Req, Err> {
        Ok(req)
    }
}

impl<F, St, Req, Err> ServiceFactory<St, Req> for FnShutdown<F, Err>
where
    F: AsyncFnOnce(&St) + Clone,
{
    type Res = Req;
    type Error = Err;

    type Service = FnShutdown<F, Err>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &St) -> Result<Self::Service, Self::InitError> {
        if let Some(f) = self.f_shutdown.take() {
            self.f_shutdown.set(Some(f.clone()));
            Ok(FnShutdown {
                f_shutdown: Cell::new(Some(f)),
                err: PhantomData,
            })
        } else {
            panic!("FnShutdown was used already");
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/fn_shutdown.rs"]
mod tests;
