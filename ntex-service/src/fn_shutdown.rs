use std::{
    cell::Cell,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use crate::Service;

#[inline]
/// Default shutdown callback for `FnShutdown` service.
pub fn f_shutdown() {}

#[inline]
/// Create `FnShutdown` service with a on_shutdown callback.
pub fn fn_shutdown<F, Fut, Req, Res, Err>(f: F) -> FnShutdown<F, Fut, Req, Res, Err>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    FnShutdown::new(f)
}

pub struct FnShutdown<F, Fut, Req, Res, Err, FShut = fn()>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    f: F,
    f_shutdown: Cell<Option<FShut>>,
    _t: PhantomData<Req>,
}

impl<F, Fut, Req, Res, Err> FnShutdown<F, Fut, Req, Res, Err>
where
    F: Fn(Req) -> Fut,
    Fut: Future<Output = Result<Res, Err>>,
{
    pub(crate) fn new(f: F) -> Self {
        Self {
            f,
            f_shutdown: Cell::new(Some(f_shutdown)),
            _t: PhantomData,
        }
    }

    /// Set function that get called on poll_shutdown method of Service trait.
    pub fn on_shutdown<FShut>(self, f: FShut) -> FnShutdown<F, Fut, Req, Res, Err, FShut>
    where
        FShut: FnOnce(),
    {
        FnShutdown {
            f: self.f,
            f_shutdown: Cell::new(Some(f)),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err, FShut> Clone for FnShutdown<F, Fut, Req, Res, Err, FShut>
where
    F: Fn(Req) -> Fut + Clone,
    FShut: FnOnce() + Clone,
    Fut: Future<Output = Result<Res, Err>>,
{
    #[inline]
    fn clone(&self) -> Self {
        let f = self.f_shutdown.take();
        self.f_shutdown.set(f.clone());

        Self {
            f: self.f.clone(),
            f_shutdown: Cell::new(f),
            _t: PhantomData,
        }
    }
}

impl<F, Fut, Req, Res, Err, FShut> Service<Req> for FnShutdown<F, Fut, Req, Res, Err, FShut>
where
    F: Fn(Req) -> Fut,
    FShut: FnOnce(),
    Fut: Future<Output = Result<Res, Err>>,
{
    type Error = Err;
    type Response = Res;
    type Future<'f> = Pin<Box<Fut>> where Self: 'f;

    #[inline]
    fn poll_shutdown(&self, _: &mut Context<'_>) -> Poll<()> {
        if let Some(f) = self.f_shutdown.take() {
            (f)()
        }
        Poll::Ready(())
    }

    #[inline]
    fn call(&self, req: Req) -> Self::Future<'_> {
        Box::pin((self.f)(req))
    }
}

#[cfg(test)]
mod tests {
    use ntex_util::future::lazy;
    use std::task::Poll;

    use super::*;

    #[ntex::test]
    async fn test_fn_shutdown() {
        let mut is_called = false;
        // The shutdown callback
        let f_shutdown = || {
            is_called = true;
        };

        let srv = fn_shutdown(|_| async { Ok::<_, ()>("srv") }).on_shutdown(f_shutdown);

        let res = srv.call(()).await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "srv");
        assert_eq!(lazy(|cx| srv.poll_shutdown(cx)).await, Poll::Ready(()));
        assert!(is_called);
    }
}
