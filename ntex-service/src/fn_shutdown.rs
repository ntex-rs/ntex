use std::{cell::Cell, convert::Infallible, fmt, marker::PhantomData};

use crate::{Ctx, Service, ServiceFactory};

#[inline]
/// Create `FnShutdown` for function that can act as a `on_shutdown` callback.
pub fn fn_shutdown<F, Err>(f: F) -> FnShutdown<F, Err>
where
    F: AsyncFnOnce(),
{
    FnShutdown::new(f)
}

pub struct FnShutdown<F, Err> {
    f_shutdown: Cell<Option<F>>,
    _t: PhantomData<Err>,
}

impl<F, Err> FnShutdown<F, Err> {
    pub(crate) fn new(f: F) -> Self {
        Self {
            f_shutdown: Cell::new(Some(f)),
            _t: PhantomData,
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
            _t: PhantomData,
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

impl<F, St, Req, Cfg, Err> ServiceFactory<St, Req, Cfg> for FnShutdown<F, Err>
where
    F: AsyncFnOnce() + Clone,
{
    type Res = Req;
    type Error = Err;

    type Service = FnShutdown<F, Err>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
        if let Some(f) = self.f_shutdown.take() {
            self.f_shutdown.set(Some(f.clone()));
            Ok(FnShutdown {
                f_shutdown: Cell::new(Some(f)),
                _t: PhantomData,
            })
        } else {
            panic!("FnShutdown was used already");
        }
    }
}

impl<F, St, Req, Err> Service<St, Req> for FnShutdown<F, Err>
where
    F: AsyncFnOnce(),
{
    type Res = Req;
    type Error = Err;

    #[inline]
    async fn shutdown(&self, _: Ctx<'_, Self, St>) {
        if let Some(f) = self.f_shutdown.take() {
            (f)().await;
        }
    }

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Req, Err> {
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use std::{future::poll_fn, rc::Rc};

    use crate::{Pipeline, factory, fn_service};

    use super::*;

    #[ntex::test]
    async fn test_fn_shutdown() {
        let is_called = Rc::new(Cell::new(false));
        let srv = fn_service(|()| async { Ok::<_, ()>("pipe") });
        let is_called2 = is_called.clone();
        let on_shutdown = fn_shutdown(async move || {
            is_called2.set(true);
        });

        let pipe = Pipeline::with(
            (),
            factory(srv)
                .and_then(on_shutdown)
                .clone()
                .create(&())
                .await
                .unwrap(),
        );

        let res = pipe.call(()).await;
        assert_eq!(pipe.ready().await, Ok(()));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "pipe");
        assert!(!pipe.is_shutdown());
        pipe.shutdown().await;
        assert!(is_called.get());
        assert!(pipe.is_shutdown());

        poll_fn(|cx| pipe.poll_shutdown(cx)).await;
        assert!(pipe.is_shutdown());

        let _ = format!("{pipe:?}");
    }
}
