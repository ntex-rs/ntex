use std::{cell::Cell, fmt, future::ready, marker::PhantomData};

use crate::{Service, ServiceCtx};

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

pub struct FnShutdownService<Req, Err, F> {
    f_shutdown: Cell<Option<F>>,
    _t: PhantomData<(Req, Err)>,
}

impl<Req, Err, F> fmt::Debug for FnShutdownService<Req, Err, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnShutdownService")
            .field("fn", &std::any::type_name::<F>())
            .finish()
    }
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

impl<Req, Err, C, F> Service<C> for FnShutdown<Req, Err, F>
where
    F: AsyncFnOnce() + Clone,
{
    type Response = FnShutdownService<Req, Err, F>;
    type Error = ();
    type Data = ();

    #[inline]
    fn call(
        &self,
        _: C,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> impl Future<Output = Result<Self::Response, Self::Error>> {
        if let Some(f) = self.f_shutdown.take() {
            self.f_shutdown.set(Some(f.clone()));
            ready(Ok(FnShutdownService {
                f_shutdown: Cell::new(Some(f)),
                _t: PhantomData,
            }))
        } else {
            panic!("FnShutdown was used already");
        }
    }
}

impl<Req, Err, F> Service<Req> for FnShutdownService<Req, Err, F>
where
    F: AsyncFnOnce(),
{
    type Response = Req;
    type Error = Err;
    type Data = ();

    #[inline]
    async fn shutdown(&self, _: &Self::Data) {
        if let Some(f) = self.f_shutdown.take() {
            (f)().await;
        }
    }

    #[inline]
    fn call(
        &self,
        req: Req,
        _: &Self::Data,
        _: ServiceCtx<'_, Self>,
    ) -> impl Future<Output = Result<Req, Err>> {
        ready(Ok(req))
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
        let srv = fn_service(|()| async { Ok::<_, ()>("pipe") });
        let is_called2 = is_called.clone();
        let on_shutdown = fn_shutdown(async move || {
            is_called2.set(true);
        });

        let pipe = chain_factory(srv)
            .and_then(on_shutdown)
            .clone()
            .pipeline((), &())
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
        poll_fn(|cx| pipe.poll_shutdown(cx)).await;
        assert!(pipe.is_shutdown());

        let _ = format!("{pipe:?}");
    }

    #[ntex::test]
    async fn test_fn_shutdown_once() {
        let is_called = Rc::new(Cell::new(false));
        let is_called2 = is_called.clone();
        let on_shutdown = fn_shutdown::<(), (), _>(async move || {
            is_called2.set(true);
        });

        let pipe = chain_factory(on_shutdown).pipeline((), &()).await.unwrap();
        pipe.shutdown().await;
        assert!(is_called.get());
        assert!(!pipe.is_shutdown());

        pipe.get_ref().shutdown(&()).await;
        assert!(is_called.get());
    }
}
