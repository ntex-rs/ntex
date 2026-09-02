use std::{convert::Infallible, fmt, marker::PhantomData};

use crate::{Ctx, Service, ServiceFactory};

/// Function that can act as a `ready` call.
pub struct FnReadiness<F, Err> {
    f: F,
    err: PhantomData<Err>,
}

impl<F, Err> FnReadiness<F, Err> {
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

impl<F, Err, St, Req, Cfg> ServiceFactory<St, Req, Cfg> for FnReadiness<F, Err>
where
    F: AsyncFn(&St) -> Result<(), Err> + Clone,
{
    type Res = Req;
    type Error = Err;

    type Service = FnReadiness<F, Err>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
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
mod tests {
    use std::{cell::Cell, rc::Rc};

    use crate::{Pipeline, factory, service};

    use super::*;

    #[ntex::test]
    async fn test_fn_readiness() {
        // Service
        let is_called = Rc::new(Cell::new(false));
        let is_called2 = is_called.clone();

        let svc = service::<_, (), _>(async |()| Ok::<_, ()>("pipe")).readiness(async move |()| {
            is_called2.set(true);
            Ok(())
        });
        let _ = format!("{svc:?}");
        let pipe = Pipeline::new(svc);

        let res = pipe.call(()).await;
        assert_eq!(pipe.ready().await, Ok(()));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "pipe");
        assert!(is_called.get());

        // Service factory
        let is_called = Rc::new(Cell::new(false));
        let is_called2 = is_called.clone();

        let factory =
            factory::<_, (), _, _>(|()| async { Ok::<_, ()>("pipe") }).readiness(async move |()| {
                is_called2.set(true);
                Ok(())
            });
        let pipe = Pipeline::new(factory.create(&()).await.unwrap());

        let res = pipe.call(()).await;
        assert_eq!(pipe.ready().await, Ok(()));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "pipe");
        assert!(is_called.get());
    }
}
