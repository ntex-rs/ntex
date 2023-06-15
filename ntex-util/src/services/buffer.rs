//! Service that buffers incomming requests.
use std::cell::{Cell, RefCell};
use std::task::{ready, Context, Poll};
use std::{collections::VecDeque, future::Future, marker::PhantomData, pin::Pin};

use ntex_service::{Ctx, IntoService, Middleware, Service, ServiceCall};

use crate::{channel::oneshot, future::Either, task::LocalWaker};

/// Buffer - service factory for service that can buffer incoming request.
///
/// Default number of buffered requests is 16
pub struct Buffer<R> {
    buf_size: usize,
    _t: PhantomData<R>,
}

impl<R> Default for Buffer<R> {
    fn default() -> Self {
        Self {
            buf_size: 16,
            _t: PhantomData,
        }
    }
}

impl<R> Buffer<R> {
    pub fn buf_size(mut self, size: usize) -> Self {
        self.buf_size = size;
        self
    }
}

impl<R> Clone for Buffer<R> {
    fn clone(&self) -> Self {
        Self {
            buf_size: self.buf_size,
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
            ready: Cell::new(false),
            waker: LocalWaker::default(),
            buf: RefCell::new(VecDeque::with_capacity(self.buf_size)),
            _t: PhantomData,
        }
    }
}

/// Buffer service - service that can buffer incoming requests.
///
/// Default number of buffered requests is 16
pub struct BufferService<R, S: Service<R>> {
    size: usize,
    ready: Cell<bool>,
    service: S,
    waker: LocalWaker,
    buf: RefCell<VecDeque<oneshot::Sender<()>>>,
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
            ready: Cell::new(false),
            service: service.into_service(),
            waker: LocalWaker::default(),
            buf: RefCell::new(VecDeque::with_capacity(size)),
            _t: PhantomData,
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
            waker: LocalWaker::default(),
            buf: RefCell::new(VecDeque::with_capacity(self.size)),
            _t: PhantomData,
        }
    }
}

impl<R, S> Service<R> for BufferService<R, S>
where
    S: Service<R>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future<'f> = Either<ServiceCall<'f, S, R>, BufferServiceResponse<'f, R, S>> where Self: 'f, R: 'f;

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
        } else if let Some(sender) = buffer.pop_front() {
            let _ = sender.send(());
            self.ready.set(false);
            Poll::Ready(Ok(()))
        } else {
            self.ready.set(true);
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    fn call<'a>(&'a self, req: R, ctx: Ctx<'a, Self>) -> Self::Future<'a> {
        if self.ready.get() {
            self.ready.set(false);
            Either::Left(ctx.call(&self.service, req))
        } else {
            let (tx, rx) = oneshot::channel();
            self.buf.borrow_mut().push_back(tx);

            Either::Right(BufferServiceResponse {
                slf: self,
                fut: ctx.call(&self.service, req),
                rx: Some(rx),
            })
        }
    }

    ntex_service::forward_poll_shutdown!(service);
}

pin_project_lite::pin_project! {
    #[doc(hidden)]
    #[must_use = "futures do nothing unless polled"]
    pub struct BufferServiceResponse<'f, R, S: Service<R>>
    {
        #[pin]
        fut: ServiceCall<'f, S, R>,
        slf: &'f BufferService<R, S>,
        rx: Option<oneshot::Receiver<()>>,
    }
}

impl<'f, R, S> Future for BufferServiceResponse<'f, R, S>
where
    S: Service<R>,
{
    type Output = Result<S::Response, S::Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        if let Some(ref rx) = this.rx {
            let _ = ready!(rx.poll_recv(cx));
            this.rx.take();
        }

        let res = ready!(this.fut.poll(cx));
        this.slf.waker.wake();
        Poll::Ready(res)
    }
}

#[cfg(test)]
mod tests {
    use ntex_service::{apply, fn_factory, Container, Service, ServiceFactory};
    use std::{rc::Rc, task::Context, task::Poll, time::Duration};

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

        fn call<'a>(&'a self, _: (), _: Ctx<'a, Self>) -> Self::Future<'a> {
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

        let srv = Container::new(BufferService::new(2, TestService(inner.clone())).clone());
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

        let srv = Container::new(BufferService::new(2, TestService(inner.clone())));
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

        let srv = srv.container(&()).await.unwrap();
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
