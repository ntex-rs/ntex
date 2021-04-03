use std::{
    cell::Cell, cell::RefCell, marker::PhantomData, pin::Pin, task::Context, task::Poll,
};

use futures_sink::Sink;

use crate::{service::Service, util::Ready};

/// `SinkService` forwards incoming requests to the provided `Sink`
pub struct SinkService<S, I> {
    sink: RefCell<S>,
    shutdown: Cell<bool>,
    _t: PhantomData<I>,
}

impl<S, I> SinkService<S, I>
where
    S: Sink<I> + Unpin,
{
    /// Create new `SinnkService` instance
    pub fn new(sink: S) -> Self {
        SinkService {
            sink: RefCell::new(sink),
            shutdown: Cell::new(false),
            _t: PhantomData,
        }
    }
}

impl<S, I> Clone for SinkService<S, I>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        SinkService {
            sink: self.sink.clone(),
            shutdown: self.shutdown.clone(),
            _t: PhantomData,
        }
    }
}

impl<S, I> Service for SinkService<S, I>
where
    S: Sink<I> + Unpin,
{
    type Request = I;
    type Response = ();
    type Error = S::Error;
    type Future = Ready<(), S::Error>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut inner = self.sink.borrow_mut();
        let pending1 = Pin::new(&mut *inner).poll_flush(cx)?.is_pending();
        let pending2 = Pin::new(&mut *inner).poll_ready(cx)?.is_pending();
        if pending1 || pending2 {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>, _: bool) -> Poll<()> {
        if !self.shutdown.get() {
            if Pin::new(&mut *self.sink.borrow_mut())
                .poll_close(cx)
                .is_pending()
            {
                Poll::Pending
            } else {
                self.shutdown.set(true);
                Poll::Ready(())
            }
        } else {
            Poll::Ready(())
        }
    }

    fn call(&self, req: I) -> Self::Future {
        Ready::from(Pin::new(&mut *self.sink.borrow_mut()).start_send(req))
    }
}
