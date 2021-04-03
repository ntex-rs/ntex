//! Service that buffers incomming requests.
use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{
    collections::VecDeque, convert::Infallible, future::Future, pin::Pin, rc::Rc,
};

use crate::channel::oneshot;
use crate::service::{IntoService, Service, Transform};
use crate::task::LocalWaker;
use crate::util::{Either, Ready};

/// Buffer - service factory for service that can buffer incoming request.
///
/// Default number of buffered requests is 16
pub struct Buffer<E> {
    buf_size: usize,
    err: Rc<dyn Fn() -> E>,
}

impl<E> Buffer<E> {
    pub fn new<F>(f: F) -> Self
    where
        F: Fn() -> E + 'static,
    {
        Self {
            buf_size: 16,
            err: Rc::new(f),
        }
    }

    pub fn buf_size(mut self, size: usize) -> Self {
        self.buf_size = size;
        self
    }
}

impl<E> Clone for Buffer<E> {
    fn clone(&self) -> Self {
        Self {
            buf_size: self.buf_size,
            err: self.err.clone(),
        }
    }
}

impl<S, E> Transform<S> for Buffer<E>
where
    S: Service<Error = E>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type InitError = Infallible;
    type Transform = BufferService<S, E>;
    type Future = Ready<Self::Transform, Self::InitError>;

    fn new_transform(&self, service: S) -> Self::Future {
        Ready::Ok(BufferService {
            size: self.buf_size,
            inner: Rc::new(Inner {
                service,
                err: self.err.clone(),
                ready: Cell::new(false),
                waker: LocalWaker::default(),
                buf: RefCell::new(VecDeque::with_capacity(self.buf_size)),
            }),
        })
    }
}

/// Buffer service - service that can buffer incoming requests.
///
/// Default number of buffered requests is 16
pub struct BufferService<S: Service<Error = E>, E> {
    size: usize,
    inner: Rc<Inner<S, E>>,
}

struct Inner<S: Service<Error = E>, E> {
    ready: Cell<bool>,
    service: S,
    waker: LocalWaker,
    err: Rc<dyn Fn() -> E>,
    buf: RefCell<VecDeque<(oneshot::Sender<S::Request>, S::Request)>>,
}

impl<S, E> BufferService<S, E>
where
    S: Service<Error = E>,
{
    pub fn new<U, F>(size: usize, err: F, service: U) -> Self
    where
        U: IntoService<S>,
        F: Fn() -> E + 'static,
    {
        Self {
            size,
            inner: Rc::new(Inner {
                err: Rc::new(err),
                ready: Cell::new(false),
                service: service.into_service(),
                waker: LocalWaker::default(),
                buf: RefCell::new(VecDeque::with_capacity(size)),
            }),
        }
    }
}

impl<S, E> Clone for BufferService<S, E>
where
    S: Service<Error = E> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            size: self.size,
            inner: Rc::new(Inner {
                err: self.inner.err.clone(),
                ready: Cell::new(false),
                service: self.inner.service.clone(),
                waker: LocalWaker::default(),
                buf: RefCell::new(VecDeque::with_capacity(self.size)),
            }),
        }
    }
}

impl<S, E> Service for BufferService<S, E>
where
    S: Service<Error = E>,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<S::Future, BufferServiceResponse<S, E>>;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let inner = self.inner.as_ref();
        inner.waker.register(cx.waker());
        let mut buffer = inner.buf.borrow_mut();

        if inner.service.poll_ready(cx)?.is_pending() {
            if buffer.len() < self.size {
                // buffer next request
                inner.ready.set(false);
                Poll::Ready(Ok(()))
            } else {
                log::trace!("Buffer limit exceeded");
                Poll::Pending
            }
        } else if let Some((sender, req)) = buffer.pop_front() {
            let _ = sender.send(req);
            inner.ready.set(false);
            Poll::Ready(Ok(()))
        } else {
            inner.ready.set(true);
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    fn poll_shutdown(&self, cx: &mut Context<'_>, is_error: bool) -> Poll<()> {
        self.inner.service.poll_shutdown(cx, is_error)
    }

    #[inline]
    fn call(&self, req: S::Request) -> Self::Future {
        if self.inner.ready.get() {
            self.inner.ready.set(false);
            Either::Left(self.inner.service.call(req))
        } else {
            let (tx, rx) = oneshot::channel();
            self.inner.buf.borrow_mut().push_back((tx, req));

            Either::Right(BufferServiceResponse {
                state: State::Tx {
                    rx,
                    inner: self.inner.clone(),
                },
            })
        }
    }
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct BufferServiceResponse<S: Service<Error = E>, E> {
        #[pin]
        state: State<S, E>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<S: Service<Error = E>, E> {
        Tx { rx: oneshot::Receiver<S::Request>, inner: Rc<Inner<S, E>> },
        Srv { #[pin] fut: S::Future, inner: Rc<Inner<S, E>> },
    }
}

impl<S: Service<Error = E>, E> Future for BufferServiceResponse<S, E> {
    type Output = Result<S::Response, S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            match this.state.project() {
                StateProject::Tx { rx, inner } => match Pin::new(rx).poll(cx) {
                    Poll::Ready(Ok(req)) => {
                        let state = State::Srv {
                            fut: inner.service.call(req),
                            inner: inner.clone(),
                        };
                        this = self.as_mut().project();
                        this.state.set(state);
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(Err((*inner.err)())),
                    Poll::Pending => return Poll::Pending,
                },
                StateProject::Srv { fut, inner } => {
                    let res = match fut.poll(cx) {
                        Poll::Ready(res) => res,
                        Poll::Pending => return Poll::Pending,
                    };
                    inner.waker.wake();
                    return Poll::Ready(res);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll};

    use super::*;
    use crate::service::{apply, fn_factory, Service, ServiceFactory};
    use crate::util::lazy;

    #[derive(Clone)]
    struct TestService(Rc<Inner>);

    struct Inner {
        ready: Cell<bool>,
        waker: LocalWaker,
        count: Cell<usize>,
    }

    impl Service for TestService {
        type Request = ();
        type Response = ();
        type Error = ();
        type Future = Ready<(), ()>;

        fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.waker.register(cx.waker());
            if self.0.ready.get() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }

        fn call(&self, _: ()) -> Self::Future {
            self.0.ready.set(false);
            self.0.count.set(self.0.count.get() + 1);
            Ready::Ok(())
        }
    }

    #[crate::rt_test]
    async fn test_transform() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv = BufferService::new(2, || (), TestService(inner.clone())).clone();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let fut1 = srv.call(());
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let fut2 = srv.call(());
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let _ = fut1.await;
        assert_eq!(inner.count.get(), 1);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let _ = fut2.await;
        assert_eq!(inner.count.get(), 2);

        let inner = Rc::new(Inner {
            ready: Cell::new(true),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv = BufferService::new(2, || (), TestService(inner.clone()));
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        let _ = srv.call(()).await;
        assert_eq!(inner.count.get(), 1);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        assert!(lazy(|cx| srv.poll_shutdown(cx, false)).await.is_ready());
    }

    #[crate::rt_test]
    async fn test_newtransform() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv = apply(
            Buffer::new(|| ()).buf_size(2).clone(),
            fn_factory(|| async { Ok(TestService(inner.clone())) }),
        );

        let srv = srv.new_service(&()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let fut1 = srv.call(());
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let fut2 = srv.call(());
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let _ = fut1.await;
        assert_eq!(inner.count.get(), 1);

        inner.ready.set(true);
        inner.waker.wake();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let _ = fut2.await;
        assert_eq!(inner.count.get(), 2);
    }
}
