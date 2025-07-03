use std::{cell::Cell, fmt, marker::PhantomData};

use crate::{Service, ServiceCtx};

#[inline]
/// Create `FnShutdown` for function that can act as a `on_shutdown` callback.
pub fn fn_shutdown<Req, Err, F>(f: F) -> FnShutdown<Req, Err, F>
where
    F: FnOnce(),
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

impl<Req, Err, F> Service<Req> for FnShutdown<Req, Err, F>
where
    F: FnOnce(),
{
    type Response = Req;
    type Error = Err;

    #[inline]
    async fn shutdown(&self) {
        if let Some(f) = self.f_shutdown.take() {
            (f)()
        }
    }

    #[inline]
    async fn call(&self, req: Req, _: ServiceCtx<'_, Self>) -> Result<Req, Err> {
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use crate::{chain, fn_service, Pipeline};

    use super::*;

    #[ntex::test]
    async fn test_fn_shutdown() {
        let is_called = Rc::new(Cell::new(false));
        let srv = fn_service(|_| async { Ok::<_, ()>("pipe") });
        let is_called2 = is_called.clone();
        let on_shutdown = fn_shutdown(|| {
            is_called2.set(true);
        });

        let pipe = Pipeline::new(chain(srv).and_then(on_shutdown).clone());

        let res = pipe.call(()).await;
        assert_eq!(pipe.ready().await, Ok(()));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "pipe");
        pipe.shutdown().await;
        assert!(is_called.get());

        let _ = format!("{pipe:?}");
    }
}
