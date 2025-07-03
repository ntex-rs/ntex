//! Service that buffers incomming requests.
use std::cell::{Cell, RefCell};
use std::task::{ready, Poll, Waker};
use std::{collections::VecDeque, fmt, future::poll_fn, marker::PhantomData};

use ntex_service::{Middleware, Pipeline, PipelineBinding, Service, ServiceCtx};

use crate::channel::oneshot;

/// Buffer - service factory for service that can buffer incoming request.
///
/// Default number of buffered requests is 16
pub struct Buffer<R> {
    buf_size: usize,
    cancel_on_shutdown: bool,
    _t: PhantomData<R>,
}

impl<R> Buffer<R> {
    pub fn buf_size(mut self, size: usize) -> Self {
        self.buf_size = size;
        self
    }

    /// Cancel all buffered requests on shutdown
    ///
    /// By default buffered requests are flushed during poll_shutdown
    pub fn cancel_on_shutdown(mut self) -> Self {
        self.cancel_on_shutdown = true;
        self
    }
}

impl<R> Default for Buffer<R> {
    fn default() -> Self {
        Self {
            buf_size: 16,
            cancel_on_shutdown: false,
            _t: PhantomData,
        }
    }
}

impl<R> fmt::Debug for Buffer<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("buf_size", &self.buf_size)
            .field("cancel_on_shutdown", &self.cancel_on_shutdown)
            .finish()
    }
}

impl<R> Clone for Buffer<R> {
    fn clone(&self) -> Self {
        Self {
            buf_size: self.buf_size,
            cancel_on_shutdown: self.cancel_on_shutdown,
            _t: PhantomData,
        }
    }
}

impl<R, S> Middleware<S> for Buffer<R>
where
    S: Service<R> + 'static,
    R: 'static,
{
    type Service = BufferService<R, S>;

    fn create(&self, service: S) -> Self::Service {
        BufferService {
            service: Pipeline::new(service).bind(),
            size: self.buf_size,
            ready: Cell::new(false),
            buf: RefCell::new(VecDeque::with_capacity(self.buf_size)),
            next_call: RefCell::default(),
            cancel_on_shutdown: self.cancel_on_shutdown,
            readiness: Cell::new(None),
            _t: PhantomData,
        }
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

impl<E: std::fmt::Display> std::fmt::Display for BufferServiceError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferServiceError::Service(e) => std::fmt::Display::fmt(e, f),
            BufferServiceError::RequestCanceled => {
                f.write_str("buffer service request canceled")
            }
        }
    }
}

impl<E: std::fmt::Display + std::fmt::Debug> std::error::Error for BufferServiceError<E> {}

/// Buffer service - service that can buffer incoming requests.
///
/// Default number of buffered requests is 16
pub struct BufferService<R, S: Service<R>> {
    size: usize,
    ready: Cell<bool>,
    service: PipelineBinding<S, R>,
    buf: RefCell<VecDeque<oneshot::Sender<oneshot::Sender<()>>>>,
    next_call: RefCell<Option<oneshot::Receiver<()>>>,
    cancel_on_shutdown: bool,
    readiness: Cell<Option<Waker>>,
    _t: PhantomData<R>,
}

impl<R, S> BufferService<R, S>
where
    S: Service<R> + 'static,
    R: 'static,
{
    pub fn new(size: usize, service: S) -> Self {
        Self {
            size,
            service: Pipeline::new(service).bind(),
            ready: Cell::new(false),
            buf: RefCell::new(VecDeque::with_capacity(size)),
            next_call: RefCell::default(),
            cancel_on_shutdown: false,
            readiness: Cell::new(None),
            _t: PhantomData,
        }
    }

    pub fn cancel_on_shutdown(self) -> Self {
        Self {
            cancel_on_shutdown: true,
            ..self
        }
    }
}

impl<R, S> Clone for BufferService<R, S>
where
    S: Service<R> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            size: self.size,
            ready: Cell::new(false),
            service: self.service.clone(),
            buf: RefCell::new(VecDeque::with_capacity(self.size)),
            next_call: RefCell::default(),
            cancel_on_shutdown: self.cancel_on_shutdown,
            readiness: Cell::new(None),
            _t: PhantomData,
        }
    }
}

impl<R, S> fmt::Debug for BufferService<R, S>
where
    S: Service<R> + fmt::Debug,
{
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

impl<R, S> Service<R> for BufferService<R, S>
where
    S: Service<R> + 'static,
    R: 'static,
{
    type Response = S::Response;
    type Error = BufferServiceError<S::Error>;

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        // hold advancement until the last released task either makes a call or is dropped
        let next_call = self.next_call.borrow_mut().take();
        if let Some(next_call) = next_call {
            let _ = next_call.recv().await;
        }

        poll_fn(|cx| {
            let mut buffer = self.buf.borrow_mut();

            // handle inner service readiness
            if self.service.poll_ready(cx)?.is_pending() {
                if buffer.len() < self.size {
                    // buffer next request
                    self.ready.set(false);
                    Poll::Ready(Ok(()))
                } else {
                    log::trace!("Buffer limit exceeded");
                    // service is not ready
                    let _ = self.readiness.take().map(|w| w.wake());
                    Poll::Pending
                }
            } else {
                while let Some(sender) = buffer.pop_front() {
                    let (next_call_tx, next_call_rx) = oneshot::channel();
                    if sender.send(next_call_tx).is_err()
                        || next_call_rx.poll_recv(cx).is_ready()
                    {
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

    async fn shutdown(&self) {
        // hold advancement until the last released task either makes a call or is dropped
        let next_call = self.next_call.borrow_mut().take();
        if let Some(next_call) = next_call {
            let _ = next_call.recv().await;
        }

        poll_fn(|cx| {
            let mut buffer = self.buf.borrow_mut();
            if self.cancel_on_shutdown {
                buffer.clear();
            }

            if !buffer.is_empty() {
                if ready!(self.service.poll_ready(cx)).is_err() {
                    log::error!(
                        "Buffered inner service failed while buffer flushing on shutdown"
                    );
                    return Poll::Ready(());
                }

                while let Some(sender) = buffer.pop_front() {
                    let (next_call_tx, next_call_rx) = oneshot::channel();
                    if sender.send(next_call_tx).is_err()
                        || next_call_rx.poll_recv(cx).is_ready()
                    {
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

        self.service.shutdown().await;
    }

    async fn call(
        &self,
        req: R,
        _: ServiceCtx<'_, Self>,
    ) -> Result<Self::Response, Self::Error> {
        if self.ready.get() {
            self.ready.set(false);
            Ok(self.service.call_nowait(req).await?)
        } else {
            let (tx, rx) = oneshot::channel();
            self.buf.borrow_mut().push_back(tx);

            // release
            let _task_guard = rx.recv().await.map_err(|_| {
                log::trace!("Buffered service request canceled");
                BufferServiceError::RequestCanceled
            })?;

            // call service
            Ok(self.service.call(req).await?)
        }
    }

    ntex_service::forward_poll!(service);
}

#[cfg(test)]
mod tests {
    use ntex_service::{apply, fn_factory, Pipeline, ServiceFactory};
    use std::{rc::Rc, time::Duration};

    use super::*;
    use crate::future::lazy;
    use crate::task::LocalWaker;

    #[derive(Debug, Clone)]
    struct TestService(Rc<Inner>);

    #[derive(Debug)]
    struct Inner {
        ready: Cell<bool>,
        waker: LocalWaker,
        count: Cell<usize>,
    }

    impl Service<()> for TestService {
        type Response = ();
        type Error = ();

        async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
            poll_fn(|cx| {
                self.0.waker.register(cx.waker());
                if self.0.ready.get() {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
            .await
        }

        async fn call(&self, _: (), _: ServiceCtx<'_, Self>) -> Result<(), ()> {
            self.0.ready.set(false);
            self.0.count.set(self.0.count.get() + 1);
            Ok(())
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_service() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv =
            Pipeline::new(BufferService::new(2, TestService(inner.clone())).clone()).bind();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.clone();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.clone();
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

        let srv = Pipeline::new(BufferService::new(2, TestService(inner.clone()))).bind();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let _ = srv.call(()).await;
        assert_eq!(inner.count.get(), 1);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(lazy(|cx| srv.poll_shutdown(cx)).await.is_ready());

        let err = BufferServiceError::from("test");
        assert!(format!("{err}").contains("test"));
        assert!(format!("{srv:?}").contains("BufferService"));
        assert!(format!("{:?}", Buffer::<TestService>::default()).contains("Buffer"));
    }

    #[ntex_macros::rt_test2]
    #[allow(clippy::redundant_clone)]
    async fn test_middleware() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv = apply(
            Buffer::default().buf_size(2).clone(),
            fn_factory(|| async { Ok::<_, ()>(TestService(inner.clone())) }),
        );

        let srv = srv.pipeline(&()).await.unwrap().bind();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.clone();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        crate::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.clone();
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
