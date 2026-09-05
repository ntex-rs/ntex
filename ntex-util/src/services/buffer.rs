//! Service that buffers incoming requests.
#![allow(clippy::type_complexity)]
use std::cell::{Cell, RefCell};
use std::{collections::VecDeque, fmt, future, marker, task, task::Poll};

use ntex_service::{Ctx, Middleware, Service, pipeline::PipelineState};

use crate::channel::oneshot;

#[derive(Copy, Clone, Debug)]
/// Buffer - service factory for service that can buffer incoming request.
///
/// Default number of buffered requests is 16
pub struct Buffer<St: Clone, Req, Res, Err> {
    buf_size: usize,
    cancel_on_shutdown: bool,
    st: marker::PhantomData<fn(St, Req) -> Result<Res, Err>>,
}

impl<St: Clone, Req, Res, Err> Buffer<St, Req, Res, Err> {
    /// Set size of the buffer.
    ///
    /// Default is set to 16
    #[must_use]
    pub fn buf_size(mut self, size: usize) -> Self {
        self.buf_size = size;
        self
    }

    /// Cancel all buffered requests on shutdown.
    ///
    /// By default buffered requests are flushed during `poll_shutdown()`
    #[must_use]
    pub fn cancel_on_shutdown(mut self) -> Self {
        self.cancel_on_shutdown = true;
        self
    }
}

impl<St: Clone, Req, Res, Err> Default for Buffer<St, Req, Res, Err> {
    fn default() -> Self {
        Self {
            buf_size: 16,
            cancel_on_shutdown: false,
            st: marker::PhantomData,
        }
    }
}

impl<S, St, Req, Res, Err> Middleware<S, St> for Buffer<St, Req, Res, Err>
where
    S: Service<St, Req, Res = Res, Error = Err> + 'static,
    St: Clone + 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    type Service = BufferService<St, Req, Res, Err>;

    fn create(&self, _: &St, service: S) -> Self::Service {
        BufferService::new(self.buf_size, PipelineState::new(service))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferServiceError<E> {
    Service(E),
    RequestCanceled,
}

impl<E> From<E> for BufferServiceError<E> {
    fn from(err: E) -> Self {
        BufferServiceError::Service(err)
    }
}

impl<E: fmt::Display> fmt::Display for BufferServiceError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BufferServiceError::Service(e) => fmt::Display::fmt(e, f),
            BufferServiceError::RequestCanceled => f.write_str("buffer service request canceled"),
        }
    }
}

impl<E: fmt::Display + fmt::Debug> std::error::Error for BufferServiceError<E> {}

/// Buffer service - service that can buffer incoming requests.
///
/// Default number of buffered requests is 16
pub struct BufferService<St, Req, Res, Err> {
    size: usize,
    ready: Cell<bool>,
    service: PipelineState<St, Req, Res, Err>,
    buf: RefCell<VecDeque<oneshot::Sender<oneshot::Sender<()>>>>,
    next_call: RefCell<Option<oneshot::Receiver<()>>>,
    cancel_on_shutdown: bool,
    readiness: Cell<Option<task::Waker>>,
}

impl<St, Req, Res, Err> BufferService<St, Req, Res, Err>
where
    St: Clone + 'static,
{
    #[must_use]
    pub fn new(size: usize, service: PipelineState<St, Req, Res, Err>) -> Self {
        Self {
            size,
            service,
            ready: Cell::new(false),
            buf: RefCell::new(VecDeque::with_capacity(size)),
            next_call: RefCell::default(),
            cancel_on_shutdown: false,
            readiness: Cell::new(None),
        }
    }

    #[must_use]
    pub fn cancel_on_shutdown(self) -> Self {
        Self {
            cancel_on_shutdown: true,
            ..self
        }
    }
}

impl<St, Req, Res, Err> fmt::Debug for BufferService<St, Req, Res, Err> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferService")
            .field("size", &self.size)
            .field("cancel_on_shutdown", &self.cancel_on_shutdown)
            .field("ready", &self.ready)
            .field("service", &self.service)
            .field("buf", &self.buf)
            .field("next_call", &self.next_call)
            .finish()
    }
}

impl<St, Req, Res, Err> Service<St, Req> for BufferService<St, Req, Res, Err>
where
    St: Clone + 'static,
    Req: 'static,
    Res: 'static,
    Err: 'static,
{
    type Res = Res;
    type Error = BufferServiceError<Err>;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        // hold advancement until the last released task either makes a call or is dropped
        let next_call = self.next_call.borrow_mut().take();
        if let Some(next_call) = next_call {
            let _ = next_call.recv().await;
        }

        ctx.poll_fn(|cx| {
            let mut buffer = self.buf.borrow_mut();

            // handle inner service readiness
            if self.service.poll_ready(cx, ctx.st())?.is_pending() {
                if buffer.len() < self.size {
                    // buffer next request
                    self.ready.set(false);
                    Poll::Ready(Ok(()))
                } else {
                    log::trace!("Buffer limit exceeded");
                    // service is not ready
                    let _ = self.readiness.take().map(task::Waker::wake);
                    Poll::Pending
                }
            } else {
                while let Some(sender) = buffer.pop_front() {
                    let (next_call_tx, next_call_rx) = oneshot::channel();
                    if sender.send(next_call_tx).is_err() || next_call_rx.poll_recv(cx).is_ready() {
                        // the task is gone
                        continue;
                    }
                    self.next_call.borrow_mut().replace(next_call_rx);
                    self.ready.set(false);
                    return Poll::Ready(Ok(()));
                }

                self.ready.set(true);
                Poll::Ready(Ok(()))
            }
        })
        .await
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        // hold advancement until the last released task either makes a call or is dropped
        let next_call = self.next_call.borrow_mut().take();
        if let Some(next_call) = next_call {
            let _ = next_call.recv().await;
        }

        future::poll_fn(|cx| {
            let mut buffer = self.buf.borrow_mut();
            if self.cancel_on_shutdown {
                buffer.clear();
            }

            if !buffer.is_empty() {
                if task::ready!(self.service.poll_ready(cx, ctx.st())).is_err() {
                    log::error!("Buffered inner service failed while buffer flushing on shutdown");
                    return Poll::Ready(());
                }

                while let Some(sender) = buffer.pop_front() {
                    let (next_call_tx, next_call_rx) = oneshot::channel();
                    if sender.send(next_call_tx).is_err() || next_call_rx.poll_recv(cx).is_ready() {
                        // the task is gone
                        continue;
                    }
                    self.next_call.borrow_mut().replace(next_call_rx);
                    if buffer.is_empty() {
                        break;
                    }
                    return Poll::Pending;
                }
            }
            Poll::Ready(())
        })
        .await;

        self.service.shutdown(ctx.st()).await;
    }

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, Self::Error> {
        if self.ready.get() {
            self.ready.set(false);
            Ok(self.service.call_nowait(req, ctx.st()).await?)
        } else {
            let (tx, rx) = oneshot::channel();
            self.buf.borrow_mut().push_back(tx);

            // release
            let _task_guard = rx.recv().await.map_err(|_| {
                log::trace!("Buffered service request canceled");
                BufferServiceError::RequestCanceled
            })?;

            // call service
            Ok(self.service.call(req, ctx.st()).await?)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use ntex_service::{Pipeline, apply, fn_factory};
    use std::{rc::Rc, time::Duration};

    use super::*;
    use crate::{future::lazy, task::LocalWaker};

    #[derive(Debug, Clone)]
    struct TestService(Rc<Inner>);

    #[derive(Debug)]
    struct Inner {
        ready: Cell<bool>,
        waker: LocalWaker,
        count: Cell<usize>,
    }

    impl Service<(), ()> for TestService {
        type Res = ();
        type Error = ();

        async fn ready(&self, ctx: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
            ctx.poll_fn(|cx| {
                self.0.waker.register(cx.waker());
                if self.0.ready.get() {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
            .await
        }

        async fn call(&self, _r: (), _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            self.0.ready.set(false);
            self.0.count.set(self.0.count.get() + 1);
            Ok(())
        }
    }

    #[ntex::test]
    async fn test_service() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let svc = BufferService::new(2, PipelineState::new(TestService(inner.clone())));
        assert!(format!("{svc:?}").contains("BufferService"));

        let srv = Pipeline::new((), svc);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 1);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 2);

        let inner = Rc::new(Inner {
            ready: Cell::new(true),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv = Pipeline::new(
            (),
            BufferService::new(2, PipelineState::new(TestService(inner.clone()))),
        );
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let _ = srv.call(()).await;
        assert_eq!(inner.count.get(), 1);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(lazy(|cx| srv.poll_shutdown(cx)).await.is_ready());

        let err = BufferServiceError::from("test");
        assert!(format!("{err}").contains("test"));
        assert!(format!("{:?}", Buffer::<(), (), (), ()>::default()).contains("Buffer"));
    }

    #[ntex::test]
    #[allow(clippy::redundant_clone)]
    async fn test_middleware() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });
        let inner2 = inner.clone();

        let srv = apply(
            Buffer::default().buf_size(2),
            fn_factory(async move |(): &()| Ok::<_, ()>(TestService(inner2.clone()))),
        );

        let srv = srv.pipeline(()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 1);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 2);
    }

    #[ntex::test]
    #[allow(clippy::redundant_clone)]
    async fn test_middleware2() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });
        let inner2 = inner.clone();

        let srv = apply(
            Buffer::default().buf_size(2),
            fn_factory(async move |(): &()| Ok::<_, ()>(TestService(inner2.clone()))),
        );

        let srv = srv.pipeline(()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 1);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 2);
    }
}
