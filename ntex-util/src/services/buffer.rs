//! Service that buffers incomming requests.
use std::cell::{Cell, RefCell};
use std::task::{Context, Poll};
use std::{collections::VecDeque, future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use ntex_service::{IntoService, Middleware, Service};

use crate::{channel::oneshot, future::Either, task::LocalWaker};

/// Buffer - service factory for service that can buffer incoming request.
///
/// Default number of buffered requests is 16
pub struct Buffer<R, E> {
    buf_size: usize,
    err: Rc<dyn Fn() -> E>,
    _t: PhantomData<R>,
}

impl<R, E> Buffer<R, E> {
    pub fn new<F>(f: F) -> Self
    where
        F: Fn() -> E + 'static,
    {
        Self {
            buf_size: 16,
            err: Rc::new(f),
            _t: PhantomData,
        }
    }

    pub fn buf_size(mut self, size: usize) -> Self {
        self.buf_size = size;
        self
    }
}

impl<R, E> Clone for Buffer<R, E> {
    fn clone(&self) -> Self {
        Self {
            buf_size: self.buf_size,
            err: self.err.clone(),
            _t: PhantomData,
        }
    }
}

impl<R, S, E> Middleware<S> for Buffer<R, E>
where
    S: Service<R, Error = E>,
{
    type Service = BufferService<R, S, E>;

    fn create(&self, service: S) -> Self::Service {
        BufferService {
            service,
            size: self.buf_size,
            err: self.err.clone(),
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            buf: RefCell::new(VecDeque::with_capacity(self.buf_size)),
        }
    }
}

/// Buffer service - service that can buffer incoming requests.
///
/// Default number of buffered requests is 16
pub struct BufferService<R, S: Service<R, Error = E>, E> {
    size: usize,
    ready: Cell<bool>,
    service: S,
    waker: LocalWaker,
    err: Rc<dyn Fn() -> E>,
    buf: RefCell<VecDeque<(oneshot::Sender<R>, R)>>,
}

impl<R, S, E> BufferService<R, S, E>
where
    S: Service<R, Error = E>,
{
    pub fn new<U, F>(size: usize, err: F, service: U) -> Self
    where
        U: IntoService<S, R>,
        F: Fn() -> E + 'static,
    {
        Self {
            size,
            err: Rc::new(err),
            ready: Cell::new(false),
            service: service.into_service(),
            waker: LocalWaker::default(),
            buf: RefCell::new(VecDeque::with_capacity(size)),
        }
    }
}

impl<R, S, E> Clone for BufferService<R, S, E>
where
    S: Service<R, Error = E> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            size: self.size,
            err: self.err.clone(),
            ready: Cell::new(false),
            service: self.service.clone(),
            waker: LocalWaker::default(),
            buf: RefCell::new(VecDeque::with_capacity(self.size)),
        }
    }
}

impl<R, S, E> Service<R> for BufferService<R, S, E>
where
    S: Service<R, Error = E>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future<'f> = Either<S::Future<'f>, BufferServiceResponse<'f, R, S, E>> where Self: 'f, R: 'f;

    #[inline]
    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.waker.register(cx.waker());
        let mut buffer = self.buf.borrow_mut();

        if self.service.poll_ready(cx)?.is_pending() {
            if buffer.len() < self.size {
                // buffer next request
                self.ready.set(false);
                Poll::Ready(Ok(()))
            } else {
                log::trace!("Buffer limit exceeded");
                Poll::Pending
            }
        } else if let Some((sender, req)) = buffer.pop_front() {
            let _ = sender.send(req);
            self.ready.set(false);
            Poll::Ready(Ok(()))
        } else {
            self.ready.set(true);
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    fn call(&self, req: R) -> Self::Future<'_> {
        if self.ready.get() {
            self.ready.set(false);
            Either::Left(self.service.call(req))
        } else {
            let (tx, rx) = oneshot::channel();
            self.buf.borrow_mut().push_back((tx, req));

            Either::Right(BufferServiceResponse {
                slf: self,
                state: State::Tx { rx },
            })
        }
    }

    ntex_service::forward_poll_shutdown!(service);
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    pub struct BufferServiceResponse<'f, R, S: Service<R, Error = E>, E>
    {
        slf: &'f BufferService<R, S, E>,
        #[pin]
        state: State<R, S::Future<'f>>,
    }
}

pin_project_lite::pin_project! {
    #[project = StateProject]
    enum State<R, F>
    where F: Future,
    {
        Tx { rx: oneshot::Receiver<R> },
        Srv { #[pin] fut: F },
    }
}

impl<'f, R, S, E> Future for BufferServiceResponse<'f, R, S, E>
where
    S: Service<R, Error = E>,
{
    type Output = Result<S::Response, S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        loop {
            match this.state.project() {
                StateProject::Tx { rx } => match Pin::new(rx).poll(cx) {
                    Poll::Ready(Ok(req)) => {
                        let state = State::Srv {
                            fut: this.slf.service.call(req),
                        };
                        this = self.as_mut().project();
                        this.state.set(state);
                    }
                    Poll::Ready(Err(_)) => return Poll::Ready(Err((*this.slf.err)())),
                    Poll::Pending => return Poll::Pending,
                },
                StateProject::Srv { fut } => {
                    let res = match fut.poll(cx) {
                        Poll::Ready(res) => res,
                        Poll::Pending => return Poll::Pending,
                    };
                    this.slf.waker.wake();
                    return Poll::Ready(res);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ntex_service::{apply, fn_factory, Service, ServiceFactory};
    use std::task::{Context, Poll};

    use super::*;
    use crate::future::{lazy, Ready};

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

        fn call(&self, _: ()) -> Self::Future<'_> {
            self.0.ready.set(false);
            self.0.count.set(self.0.count.get() + 1);
            Ready::Ok(())
        }
    }

    #[ntex_macros::rt_test2]
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
        assert!(lazy(|cx| srv.poll_shutdown(cx)).await.is_ready());
    }

    #[ntex_macros::rt_test2]
    async fn test_newtransform() {
        let inner = Rc::new(Inner {
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            count: Cell::new(0),
        });

        let srv = apply(
            Buffer::new(|| ()).buf_size(2).clone(),
            fn_factory(|| async { Ok::<_, ()>(TestService(inner.clone())) }),
        );

        let srv = srv.create(&()).await.unwrap();
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
