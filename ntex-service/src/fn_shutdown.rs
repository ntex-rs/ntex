use std::{cell::Cell, fmt, marker::PhantomData};

use crate::{Service, ServiceCtx, ServiceFactory};

#[inline]
/// Create `FnShutdown` for function that can act as a `on_shutdown` callback.
pub fn fn_shutdown<Req, Err, F>(f: F) -> FnShutdown<Req, Err, F>
where
    F: AsyncFnOnce(),
{
    FnShutdown::new(f)
}

pub struct FnShutdown<Req, Err, F> {
    f_shutdown: Cell<Option<F>>,
    _t: PhantomData<(Req, Err)>,
}

impl<Req, Err, F> FnShutdown<Req, Err, F> {
    pub(crate) fn new(f: F) -> Self {
        Self {
            f_shutdown: Cell::new(Some(f)),
            _t: PhantomData,
        }
    }
}

impl<Req, Err, F> Clone for FnShutdown<Req, Err, F>
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

impl<Req, Err, F> fmt::Debug for FnShutdown<Req, Err, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnShutdown")
            .field("fn", &std::any::type_name::<F>())
            .finish()
    }
}

impl<Req, Err, C, F> ServiceFactory<Req, C> for FnShutdown<Req, Err, F>
where
    F: AsyncFnOnce() + Clone,
{
    type Response = Req;
    type Error = Err;
    type Service = FnShutdown<Req, Err, F>;
    type InitError = ();

    #[inline]
    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
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

impl<Req, Err, F> Service<Req> for FnShutdown<Req, Err, F>
where
    F: AsyncFnOnce(),
{
    type Response = Req;
    type Error = Err;

    #[inline]
    async fn shutdown(&self) {
        if let Some(f) = self.f_shutdown.take() {
            (f)().await;
        }
    }

    #[inline]
    async fn call(&self, req: Req, _: ServiceCtx<'_, Self>) -> Result<Req, Err> {
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use std::{future::poll_fn, rc::Rc};

    use crate::{chain_factory, fn_service};

    use super::*;

    #[ntex::test]
    async fn test_fn_shutdown() {
        let is_called = Rc::new(Cell::new(false));
        let srv = fn_service(|_| async { Ok::<_, ()>("pipe") });
        let is_called2 = is_called.clone();
        let on_shutdown = fn_shutdown(async move || {
            is_called2.set(true);
        });

        let pipe = chain_factory(srv)
            .and_then(on_shutdown)
            .clone()
            .pipeline(())
            .await
            .unwrap();

        let res = pipe.call(()).await;
        assert_eq!(pipe.ready().await, Ok(()));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "pipe");
        assert!(!pipe.is_shutdown());
        pipe.shutdown().await;
        assert!(is_called.get());
        assert!(!pipe.is_shutdown());

        let pipe = pipe.bind();
        let _ = poll_fn(|cx| pipe.poll_shutdown(cx)).await;
        assert!(pipe.is_shutdown());

        let _ = format!("{pipe:?}");
    }

    #[ntex::test]
    #[should_panic]
    async fn test_fn_shutdown_panic() {
        let is_called = Rc::new(Cell::new(false));
        let is_called2 = is_called.clone();
        let on_shutdown = fn_shutdown::<(), (), _>(async move || {
            is_called2.set(true);
        });

        let pipe = chain_factory(on_shutdown).pipeline(()).await.unwrap();
        pipe.shutdown().await;
        assert!(is_called.get());
        assert!(!pipe.is_shutdown());

        let _factory = pipe.get_ref().create(()).await;
    }
}
