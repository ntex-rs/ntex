#![allow(clippy::unused_unit)]
use std::{cell::Cell, fmt, future::Future, pin::Pin, task::Context, task::Poll};

/// Execute fn during drop
pub struct OnDropFn<F: FnOnce()> {
    f: Cell<Option<F>>,
}

impl<F: FnOnce()> OnDropFn<F> {
    pub fn new(f: F) -> Self {
        Self {
            f: Cell::new(Some(f)),
        }
    }

    /// Cancel fn execution
    pub fn cancel(&self) {
        self.f.take();
    }
}

impl<F: FnOnce()> fmt::Debug for OnDropFn<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnDropFn")
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F: FnOnce()> Drop for OnDropFn<F> {
    fn drop(&mut self) {
        if let Some(f) = self.f.take() {
            f()
        }
    }
}

/// Trait adds future on_drop support
pub trait OnDropFutureExt: Future + Sized {
    fn on_drop<F: FnOnce()>(self, on_drop: F) -> OnDropFuture<Self, F> {
        OnDropFuture::new(self, on_drop)
    }
}

impl<F: Future> OnDropFutureExt for F {}

pin_project_lite::pin_project! {
    pub struct OnDropFuture<Ft: Future, F: FnOnce()> {
        #[pin]
        fut: Ft,
        on_drop: OnDropFn<F>
    }
}

impl<Ft: Future, F: FnOnce()> OnDropFuture<Ft, F> {
    pub fn new(fut: Ft, on_drop: F) -> Self {
        Self {
            fut,
            on_drop: OnDropFn::new(on_drop),
        }
    }
}

impl<Ft: Future, F: FnOnce()> Future for OnDropFuture<Ft, F> {
    type Output = Ft::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.fut.poll(cx) {
            Poll::Ready(r) => {
                this.on_drop.cancel();
                Poll::Ready(r)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod test {
    use std::future::{pending, poll_fn};

    use super::*;

    #[ntex::test]
    async fn on_drop() {
        let f = OnDropFn::new(|| ());
        assert!(format!("{f:?}").contains("OnDropFn"));
        f.cancel();
        assert!(f.f.get().is_none());

        let mut dropped = false;
        let mut f = pending::<()>().on_drop(|| {
            dropped = true;
        });
        poll_fn(|cx| {
            let _ = Pin::new(&mut f).poll(cx);
            Poll::Ready(())
        })
        .await;

        drop(f);
        assert!(dropped);
    }
}
