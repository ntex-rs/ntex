use std::{
    cell::Cell,
    marker::PhantomData,
    task::{Context, Poll}, pin::Pin, future::Future,
};

use crate::Service;

#[inline]
/// Create `FnShutdown` service with a on_shutdown callback.
pub fn fn_shutdown<Req, Err, F>(f: F) -> FnShutdown<Req, Err, F>
where
    F: FnOnce(),
{
    FnShutdown::new(f)
}

pub struct FnShutdown<Req, Err, F = fn()>
where {
    f_shutdown: Cell<Option<F>>,
    _f: PhantomData<Req>,
    _e: PhantomData<Err>,
}

impl<Req, Err, F> FnShutdown<Req, Err, F>
where
    F: FnOnce(),
{
    pub(crate) fn new(f: F) -> Self {
        Self {
            f_shutdown: Cell::new(Some(f)),
            _f: PhantomData,
            _e: PhantomData,
        }
    }
}

impl<Req, Err, F> Service<Req> for FnShutdown<Req, Err, F>
where
    F: FnOnce(),
    Req: Clone + 'static,
{
    type Response = Req;
    type Error = Err;
    type Future<'f> = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>> where Self: 'f;

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
        if let Some(f) = self.f_shutdown.take() {
            (f)()
        }
        Poll::Ready(())
    }

    fn call(&self, req: Req) -> Self::Future<'_> {
        Box::pin(async move { Ok(req) })
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::lazy;
    use std::task::Poll;

    use crate::{pipeline, fn_service};

    use super::*;

    #[ntex::test]
    async fn test_fn_shutdown() {
        let mut is_called = false;

        let pipe = pipeline(fn_service(|_| async { Ok::<_, ()>("pipe") }))
            .and_then(fn_shutdown(|| { is_called = true }));

        let res = pipe.call(()).await;
        assert_eq!(lazy(|cx| pipe.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "pipe");
        assert_eq!(lazy(|cx| pipe.poll_shutdown(cx)).await, Poll::Ready(()));
        assert!(is_called);
    }
}
