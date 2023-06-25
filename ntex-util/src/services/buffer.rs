//! Service that buffers incomming requests.
use std::cell::{Cell, RefCell};
use std::task::{ready, Context, Poll};
use std::{collections::VecDeque, future::Future, marker::PhantomData, pin::Pin};

use ntex_service::{IntoService, Middleware, Service, ServiceCallToCall, ServiceCtx};

use crate::channel::{oneshot, Canceled};

/// Buffer - service factory for service that can buffer incoming request.
///
/// Default number of buffered requests is 16
pub struct Buffer<R> {
    buf_size: usize,
    cancel_on_shutdown: bool,
    _t: PhantomData<R>,
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
    S: Service<R>,
{
    type Service = BufferService<R, S>;

    fn create(&self, service: S) -> Self::Service {
        BufferService {
            service,
            size: self.buf_size,
            cancel_on_shutdown: self.cancel_on_shutdown,
            ready: Cell::new(false),
            buf: RefCell::new(VecDeque::with_capacity(self.buf_size)),
            next_call: RefCell::default(),
            _t: PhantomData,
        }
    }
}

/// Buffer service - service that can buffer incoming requests.
///
/// Default number of buffered requests is 16
pub struct BufferService<R, S: Service<R>> {
    size: usize,
    cancel_on_shutdown: bool,
    ready: Cell<bool>,
    service: S,
    buf: RefCell<VecDeque<oneshot::Sender<oneshot::Sender<()>>>>,
    next_call: RefCell<Option<oneshot::Receiver<()>>>,
    _t: PhantomData<R>,
}

impl<R, S> BufferService<R, S>
where
    S: Service<R>,
{
    pub fn new<U>(size: usize, service: U) -> Self
    where
        U: IntoService<S, R>,
    {
        Self {
            size,
            cancel_on_shutdown: false,
            ready: Cell::new(false),
            service: service.into_service(),
            buf: RefCell::new(VecDeque::with_capacity(size)),
            next_call: RefCell::default(),
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
            cancel_on_shutdown: self.cancel_on_shutdown,
            ready: Cell::new(false),
            service: self.service.clone(),
            buf: RefCell::new(VecDeque::with_capacity(self.size)),
            next_call: RefCell::default(),
            _t: PhantomData,
        }
    }
}

impl<R, S> Service<R> for BufferService<R, S>
where
    S: Service<R>,
{
    type Response = S::Response;
    type Error = BufferServiceError<S::Error>;
    type Future<'f> = BufferServiceResponse<'f, R, S> where Self: 'f, R: 'f;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut buffer = self.buf.borrow_mut();
        let mut next_call = self.next_call.borrow_mut();
        if let Some(next_call) = &*next_call {
            // hold advancement until the last released task either makes a call or is dropped
            let _ = ready!(next_call.poll_recv(cx));
        }
        next_call.take();

        if self.service.poll_ready(cx)?.is_pending() {
            if buffer.len() < self.size {
                // buffer next request
                self.ready.set(false);
                return Poll::Ready(Ok(()));
            } else {
                log::trace!("Buffer limit exceeded");
                return Poll::Pending;
            }
        }

        while let Some(sender) = buffer.pop_front() {
            let (next_call_tx, next_call_rx) = oneshot::channel();
            if sender.send(next_call_tx).is_err() || next_call_rx.poll_recv(cx).is_ready() {
                // the task is gone
                continue;
            }
            next_call.replace(next_call_rx);
            self.ready.set(false);
            return Poll::Ready(Ok(()));
        }

        self.ready.set(true);
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call<'a>(&'a self, req: R, ctx: ServiceCtx<'a, Self>) -> Self::Future<'a> {
        if self.ready.get() {
            self.ready.set(false);
            BufferServiceResponse {
                slf: self,
                state: ResponseState::Running {
                    fut: ctx.call_nowait(&self.service, req),
                },
            }
        } else {
            let (tx, rx) = oneshot::channel();
            self.buf.borrow_mut().push_back(tx);

            BufferServiceResponse {
                slf: self,
                state: ResponseState::WaitingForRelease {
                    rx,
                    call: Some(ctx.call(&self.service, req).advance_to_call()),
                },
            }
        }
    }

    fn poll_shutdown(&self, cx: &mut std::task::Context<'_>) -> Poll<()> {
        let mut buffer = self.buf.borrow_mut();
        if self.cancel_on_shutdown {
            buffer.clear();
        } else if !buffer.is_empty() {
            let mut next_call = self.next_call.borrow_mut();
            if let Some(next_call) = &*next_call {
                // hold advancement until the last released task either makes a call or is dropped
                let _ = ready!(next_call.poll_recv(cx));
            }
            next_call.take();

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
                next_call.replace(next_call_rx);
                if buffer.is_empty() {
                    break;
                }
                return Poll::Pending;
            }
        }

        self.service.poll_shutdown(cx)
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    #[must_use = "futures do nothing unless polled"]
    pub struct BufferServiceResponse<'f, R, S: Service<R>>
    {
        #[pin]
        state: ResponseState<'f, R, S>,
        slf: &'f BufferService<R, S>,
    }
}

pin_project_lite::pin_project! {
    #[project = ResponseStateProject]
    enum ResponseState<'f, R, S: Service<R>>
    {
        WaitingForRelease { rx: oneshot::Receiver<oneshot::Sender<()>>, call: Option<ServiceCallToCall<'f, S, R>> },
        WaitingForReady { tx: oneshot::Sender<()>, #[pin] call: ServiceCallToCall<'f, S, R> },
        Running { #[pin] fut: S::Future<'f> },
    }
}

impl<'f, R, S> Future for BufferServiceResponse<'f, R, S>
where
    S: Service<R>,
{
    type Output = Result<S::Response, BufferServiceError<S::Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();
        match this.state.as_mut().project() {
            ResponseStateProject::WaitingForRelease { rx, call } => {
                match ready!(rx.poll_recv(cx)) {
                    Ok(tx) => {
                        let call = call.take().expect("always set in this state");
                        this.state.set(ResponseState::WaitingForReady { tx, call });
                        self.poll(cx)
                    }
                    Err(Canceled) => {
                        log::trace!("Buffered service request cancelled");
                        Poll::Ready(Err(BufferServiceError::RequestCanceled))
                    }
                }
            }
            ResponseStateProject::WaitingForReady { call, .. } => {
                let fut = match ready!(call.poll(cx)) {
                    Ok(fut) => fut,
                    Err(err) => return Poll::Ready(Err(err.into())),
                };

                this.state.set(ResponseState::Running { fut });
                self.poll(cx)
            }
            ResponseStateProject::Running { fut } => fut.poll(cx).map_err(|e| e.into()),
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

#[cfg(test)]
mod tests {
    use ntex_service::{apply, fn_factory, Pipeline, Service, ServiceFactory};
    use std::{rc::Rc, task::Context, task::Poll, time::Duration};

    use super::*;
    use crate::future::{lazy, Ready};
    use crate::task::LocalWaker;

    #[derive(Clone)]
    struct TestService(Rc<Inner>);

    struct Inner {
        ready: Cell<bool>,
        waker: LocalWaker,
        count: Cell<usize>,
    }

    impl Service<()> for TestService {
        type Response = ();
        type Error = ();
        type Future<'f> = Ready<(), ()> where Self: 'f;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.waker.register(cx.waker());
            if self.0.ready.get() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn call<'a>(&'a self, _: (), _: ServiceCtx<'a, Self>) -> Self::Future<'a> {
            self.0.ready.set(false);
            self.0.count.set(self.0.count.get() + 1);
            Ready::Ok(())
        }
    }

    #[ntex_macros::rt_test2]
    async fn test_service() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv = Pipeline::new(BufferService::new(2, TestService(inner.clone())).clone());
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

        let srv = Pipeline::new(BufferService::new(2, TestService(inner.clone())));
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        let _ = srv.call(()).await;
        assert_eq!(inner.count.get(), 1);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        assert!(lazy(|cx| srv.poll_shutdown(cx)).await.is_ready());
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

        let srv = srv.pipeline(&()).await.unwrap();
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
