//! Service that buffers incoming requests.
#![allow(clippy::type_complexity)]
use std::cell::{Cell, RefCell};
use std::task::Poll;
use std::{collections::VecDeque, fmt, future, future::Future, marker, mem, pin, rc::Rc};

use ntex_service::{BindCtx, Ctx, Middleware, ReadyNotifier, Service, pipeline::PipelineState};

use crate::{channel::oneshot, future::OnDropFn, task::LocalWaker};

#[derive(Copy, Clone, Debug)]
/// Middleware that buffers requests while the wrapped service is not ready.
///
/// See [`BufferService`] for how buffered requests are released.
///
/// The default buffer capacity is 16 requests.
pub struct Buffer<St: Clone, Req, Res, Err> {
    buf_size: usize,
    cancel_on_shutdown: bool,
    st: marker::PhantomData<fn(St, Req) -> Result<Res, Err>>,
}

impl<St: Clone, Req, Res, Err> Buffer<St, Req, Res, Err> {
    /// Sets the maximum number of buffered requests.
    ///
    /// The default capacity is 16.
    #[must_use]
    pub fn buf_size(mut self, size: usize) -> Self {
        self.buf_size = size;
        self
    }

    /// Cancel all buffered requests on shutdown.
    ///
    /// By default, buffered requests are released during service shutdown.
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
        let service = BufferService::new(self.buf_size, PipelineState::new(service));
        if self.cancel_on_shutdown {
            service.cancel_on_shutdown()
        } else {
            service
        }
    }
}

/// Errors returned by [`BufferService`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferServiceError<E> {
    /// Error returned by the wrapped service.
    Service(E),
    /// The buffered request was canceled before it could be processed.
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

/// A service that buffers requests while its wrapped service is not ready.
///
/// The service is ready while the buffer has room; it does not depend on the
/// readiness of the wrapped service. Requests that arrive while the wrapped
/// service is not ready, or while earlier requests are still waiting, are
/// buffered and released in order. Each released request waits for the wrapped
/// service's readiness, then releases the next one before calling the wrapped
/// service. The wrapped service must notify synchronously when a call consumes
/// its capacity, as required by readiness caching.
///
/// The buffer size is a soft limit: callers that call without a readiness
/// check, or after an outdated one, are still buffered.
/// Queue capacity changes invalidate cached readiness for every pipeline sharing
/// this service, including when a queued request is canceled.
///
/// Errors of the wrapped service's readiness are returned to the waiting
/// request. Shutdown waits until all buffered requests have completed their
/// calls, unless [`cancel_on_shutdown`](Self::cancel_on_shutdown) is set, and
/// shuts down the wrapped service afterwards.
///
/// The default buffer capacity is 16 requests. A capacity of 0 is treated as 1.
pub struct BufferService<St, Req, Res, Err> {
    shared: Rc<Shared>,
    service: PipelineState<St, Req, Res, Err>,
    cancel_on_shutdown: bool,
}

struct Shared {
    size: usize,
    buf: RefCell<VecDeque<oneshot::Sender<Token>>>,
    // a released request holds the token while it waits for inner readiness
    has_token: Cell<bool>,
    // buffered requests in the inner call
    in_flight: Cell<usize>,
    canceled: Cell<bool>,
    notifier: ReadyNotifier,
    token_waker: LocalWaker,
    drained: LocalWaker,
}

impl Shared {
    fn has_room(&self) -> bool {
        self.buf.borrow().len() < self.size
    }

    fn is_drained(&self) -> bool {
        !self.has_token.get() && self.in_flight.get() == 0 && self.buf.borrow().is_empty()
    }

    /// Passes the token to the next waiting request.
    fn release_next(self: &Rc<Self>) {
        loop {
            let (sender, freed) = {
                let mut buf = self.buf.borrow_mut();
                let full = buf.len() >= self.size;
                let sender = buf.pop_front();
                (sender, full && buf.len() < self.size)
            };
            let Some(sender) = sender else {
                self.has_token.set(false);
                self.drained.wake();
                return;
            };
            let token = Token(Some(self.clone()));
            if freed {
                self.notifier.notify();
            }
            match sender.send(token) {
                Ok(()) => return,
                // the request is gone
                Err(token) => token.disarm(),
            }
        }
    }

    /// Removes dropped requests from the buffer.
    fn prune(&self) {
        let freed = {
            let mut buf = self.buf.borrow_mut();
            let full = buf.len() >= self.size;
            buf.retain(|sender| !sender.is_canceled());
            full && buf.len() < self.size
        };
        if freed {
            self.notifier.notify();
        }
    }
}

/// The right to wait for inner readiness; dropping it releases the next request.
struct Token(Option<Rc<Shared>>);

impl Token {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for Token {
    fn drop(&mut self) {
        if let Some(shared) = self.0.take() {
            shared.release_next();
        }
    }
}

/// Removes a dropped buffered request from the buffer.
struct Waiter<'a> {
    shared: &'a Shared,
    rx: Option<oneshot::Receiver<Token>>,
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        if let Some(rx) = self.rx.take() {
            // drops a token that was sent but not received
            drop(rx);
            self.shared.prune();
        }
    }
}

struct InFlight<'a>(&'a Shared);

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.in_flight.set(self.0.in_flight.get() - 1);
        self.0.drained.wake();
    }
}

impl<St, Req, Res, Err> BufferService<St, Req, Res, Err>
where
    St: Clone + 'static,
{
    #[must_use]
    /// Creates a buffering service with the specified capacity.
    pub fn new(size: usize, service: PipelineState<St, Req, Res, Err>) -> Self {
        let size = size.max(1);
        Self {
            shared: Rc::new(Shared {
                size,
                buf: RefCell::new(VecDeque::with_capacity(size)),
                has_token: Cell::new(false),
                in_flight: Cell::new(0),
                canceled: Cell::new(false),
                notifier: ReadyNotifier::new(),
                token_waker: LocalWaker::new(),
                drained: LocalWaker::new(),
            }),
            service,
            cancel_on_shutdown: false,
        }
    }

    #[must_use]
    /// Cancels pending buffered requests instead of releasing them on shutdown.
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
            .field("size", &self.shared.size)
            .field("cancel_on_shutdown", &self.cancel_on_shutdown)
            .field("buffered", &self.shared.buf.borrow().len())
            .field("in_flight", &self.shared.in_flight.get())
            .field("service", &self.service)
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

    fn bind(&self, ctx: BindCtx<'_>) {
        // readiness depends on the buffer only, the inner pipeline is not bound
        ctx.subscribe(&self.shared.notifier);
    }

    async fn ready(&self, _: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        if self.shared.has_room() {
            Ok(())
        } else {
            log::trace!("Buffer limit exceeded");
            // releasing capacity notifies and restarts the check
            future::pending().await
        }
    }

    async fn shutdown(&self, ctx: Ctx<'_, Self, St>) {
        let shared = &self.shared;
        if self.cancel_on_shutdown {
            shared.canceled.set(true);
            let buf = mem::take(&mut *shared.buf.borrow_mut());
            let freed = buf.len() >= shared.size;
            let _cancel = OnDropFn::new(|| {
                shared.token_waker.wake();
                // buffered requests get `RequestCanceled`
                drop(buf);
            });
            if freed {
                shared.notifier.notify();
            }
        }

        future::poll_fn(|cx| {
            if shared.is_drained() {
                return Poll::Ready(());
            }
            shared.drained.register(cx.waker());
            if shared.is_drained() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        self.service.shutdown(ctx.st()).await;
    }

    async fn call(&self, req: Req, ctx: Ctx<'_, Self, St>) -> Result<Res, Self::Error> {
        let shared = &self.shared;
        let idle = !shared.has_token.get() && shared.buf.borrow().is_empty();
        if idle && self.service.is_ready() {
            return Ok(self.service.call(req, ctx.st()).await?);
        }

        let (token, mut buffered) = if idle {
            shared.has_token.set(true);
            (Token(Some(shared.clone())), false)
        } else {
            let (tx, rx) = oneshot::channel();
            let full = {
                let mut buf = shared.buf.borrow_mut();
                buf.push_back(tx);
                buf.len() == shared.size
            };
            let mut waiter = Waiter {
                shared,
                rx: Some(rx),
            };
            if full {
                shared.notifier.notify();
            }
            let result = future::poll_fn(|cx| waiter.rx.as_ref().unwrap().poll_recv(cx)).await;
            waiter.rx = None;
            let token = result.map_err(|_| {
                log::trace!("Buffered service request canceled");
                BufferServiceError::RequestCanceled
            })?;
            (token, true)
        };

        // the token holder waits for inner readiness, dropping it releases the next request
        let mut ready = pin::pin!(self.service.ready(ctx.st()));
        future::poll_fn(|cx| {
            if shared.canceled.get() {
                return Poll::Ready(Err(BufferServiceError::RequestCanceled));
            }
            if let Poll::Ready(result) = ready.as_mut().poll(cx) {
                Poll::Ready(result.map_err(BufferServiceError::Service))
            } else {
                buffered = true;
                shared.token_waker.register(cx.waker());
                Poll::Pending
            }
        })
        .await?;

        let _in_flight = buffered.then(|| {
            shared.in_flight.set(shared.in_flight.get() + 1);
            InFlight(shared)
        });
        drop(token);

        Ok(self.service.call(req, ctx.st()).await?)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unused_async_trait_impl)]
    use ntex_service::{Pipeline, ReadyCallback, apply, fn_factory};
    use std::{cell::RefCell, pin::Pin, time::Duration};
    use std::{panic::AssertUnwindSafe, panic::catch_unwind, task::Context};

    use super::*;
    use crate::future::lazy;

    #[derive(Debug, Clone)]
    struct TestService(Rc<Inner>);

    #[derive(Debug, Default)]
    struct Inner {
        ready: Cell<bool>,
        waker: LocalWaker,
        changed: ReadyNotifier,
        count: Cell<usize>,
    }

    impl Inner {
        fn new(ready: bool) -> Rc<Self> {
            let inner = Rc::new(Self::default());
            inner.ready.set(ready);
            inner
        }

        fn set_ready(&self) {
            self.ready.set(true);
            self.waker.wake();
        }
    }

    impl Service<(), ()> for TestService {
        type Res = ();
        type Error = ();

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.0.changed);
        }

        async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), Self::Error> {
            future::poll_fn(|cx| {
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
            // each call consumes readiness
            self.0.ready.set(false);
            self.0.changed.notify();
            self.0.count.set(self.0.count.get() + 1);
            Ok(())
        }
    }

    fn spawn_call(
        srv: &Pipeline<(), (), BufferServiceError<()>>,
    ) -> Rc<RefCell<Option<Result<(), BufferServiceError<()>>>>> {
        let result = Rc::new(RefCell::new(None));
        let result2 = result.clone();
        let srv = srv.bind();
        ntex::rt::spawn(async move {
            *result2.borrow_mut() = Some(srv.call(()).await);
        });
        result
    }

    async fn settle() {
        crate::time::sleep(Duration::from_millis(25)).await;
    }

    #[ntex::test]
    async fn test_service() {
        let inner = Inner::new(false);
        let svc = BufferService::new(2, PipelineState::new(TestService(inner.clone())));
        assert!(format!("{svc:?}").contains("BufferService"));

        let srv = Pipeline::new((), svc);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        // the first request waits for inner readiness, the others are buffered
        let _r1 = spawn_call(&srv);
        let _r2 = spawn_call(&srv);
        settle().await;
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        let _r3 = spawn_call(&srv);
        settle().await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        // requests are released one per inner readiness
        inner.set_ready();
        settle().await;
        assert_eq!(inner.count.get(), 1);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        inner.set_ready();
        settle().await;
        assert_eq!(inner.count.get(), 2);

        inner.set_ready();
        settle().await;
        assert_eq!(inner.count.get(), 3);
        assert!(lazy(|cx| srv.poll_shutdown(cx)).await.is_ready());

        let err = BufferServiceError::from("test");
        assert!(format!("{err}").contains("test"));
        assert!(format!("{:?}", Buffer::<(), (), (), ()>::default()).contains("Buffer"));
    }

    #[ntex::test]
    async fn sequential_calls_with_ready_service() {
        let inner = Rc::new(PendingInner::default());
        let srv = Pipeline::new(
            (),
            BufferService::new(2, PipelineState::new(ReadyService(inner.clone()))),
        );

        for _ in 0..3 {
            crate::time::timeout(Duration::from_secs(1), srv.call(()))
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(inner.count.get(), 3);
    }

    #[ntex::test]
    #[allow(clippy::redundant_clone)]
    async fn test_middleware() {
        let inner = Inner::new(false);
        let inner2 = inner.clone();

        let srv = apply(
            Buffer::default().buf_size(1),
            fn_factory(async move |(): &()| Ok::<_, ()>(TestService(inner2.clone()))),
        );

        let srv = srv.pipeline(()).await.unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        let srv1 = srv.bind();
        ntex::rt::spawn(async move {
            let _ = srv1.call(()).await;
        });
        settle().await;
        assert_eq!(inner.count.get(), 0);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        inner.set_ready();
        settle().await;
        assert_eq!(inner.count.get(), 1);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        inner.set_ready();
        settle().await;
        assert_eq!(inner.count.get(), 2);
    }

    #[ntex::test]
    async fn middleware_cancels_buffered_requests_on_shutdown() {
        let inner = Inner::new(false);
        let inner2 = inner.clone();

        let srv = apply(
            Buffer::default().buf_size(1).cancel_on_shutdown(),
            fn_factory(async move |(): &()| Ok::<_, ()>(TestService(inner2.clone()))),
        )
        .pipeline(())
        .await
        .unwrap();
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        // one request waits for inner readiness, one is buffered
        let r1 = spawn_call(&srv);
        let r2 = spawn_call(&srv);
        settle().await;
        crate::time::timeout(Duration::from_secs(1), srv.shutdown())
            .await
            .unwrap();
        settle().await;

        assert!(matches!(
            *r1.borrow(),
            Some(Err(BufferServiceError::RequestCanceled))
        ));
        assert!(matches!(
            *r2.borrow(),
            Some(Err(BufferServiceError::RequestCanceled))
        ));
        assert_eq!(inner.count.get(), 0);
    }

    #[derive(Default)]
    struct PendingInner {
        started: Cell<bool>,
        completed: Cell<bool>,
        shutdown: Cell<bool>,
        count: Cell<usize>,
    }

    struct ReadyService(Rc<PendingInner>);

    impl Service<(), ()> for ReadyService {
        type Res = ();
        type Error = ();

        async fn call(&self, (): (), _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            self.0.count.set(self.0.count.get() + 1);
            Ok(())
        }
    }

    struct PendingService {
        inner: Rc<PendingInner>,
        release: RefCell<Option<oneshot::Receiver<()>>>,
    }

    impl Service<(), ()> for PendingService {
        type Res = ();
        type Error = ();

        async fn call(&self, (): (), _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            self.inner.started.set(true);
            let release = self.release.borrow_mut().take().unwrap();
            release.recv().await.unwrap();
            self.inner.completed.set(true);
            Ok(())
        }

        async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
            self.inner.shutdown.set(true);
        }
    }

    #[ntex::test]
    async fn shutdown_with_in_flight_request() {
        let inner = Rc::new(PendingInner::default());
        let (release_tx, release_rx) = oneshot::channel();
        let service = PendingService {
            inner: inner.clone(),
            release: RefCell::new(Some(release_rx)),
        };
        let srv = Pipeline::new((), BufferService::new(1, PipelineState::new(service)));

        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let srv2 = srv.bind();
        ntex::rt::spawn(async move {
            srv2.call(()).await.unwrap();
        });
        settle().await;

        // the request was not buffered
        assert!(inner.started.get());
        assert!(!inner.completed.get());
        assert!(lazy(|cx| srv.poll_shutdown(cx)).await.is_ready());
        assert!(inner.shutdown.get());
        assert!(!inner.completed.get());

        release_tx.send(()).unwrap();
        settle().await;

        assert!(inner.completed.get());
    }

    #[derive(Default)]
    struct DrainInner {
        ready: Cell<bool>,
        waker: LocalWaker,
        active: Cell<usize>,
        count: Cell<usize>,
        active_on_shutdown: Cell<Option<usize>>,
    }

    struct DrainService(Rc<DrainInner>);

    impl Service<(), ()> for DrainService {
        type Res = ();
        type Error = ();

        async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            future::poll_fn(|cx| {
                self.0.waker.register(cx.waker());
                if self.0.ready.get() {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
            .await
        }

        async fn call(&self, (): (), _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            let inner = &self.0;
            inner.active.set(inner.active.get() + 1);
            crate::time::sleep(Duration::from_millis(25)).await;
            inner.active.set(inner.active.get() - 1);
            inner.count.set(inner.count.get() + 1);
            Ok(())
        }

        async fn shutdown(&self, _: Ctx<'_, Self, ()>) {
            self.0.active_on_shutdown.set(Some(self.0.active.get()));
        }
    }

    #[ntex::test]
    async fn shutdown_drains_buffer() {
        let inner = Rc::new(DrainInner::default());
        let srv = Pipeline::new(
            (),
            BufferService::new(4, PipelineState::new(DrainService(inner.clone()))),
        );

        for _ in 0..2 {
            srv.ready().await.unwrap();
            let srv = srv.bind();
            ntex::rt::spawn(async move {
                srv.call(()).await.unwrap();
            });
        }
        crate::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(inner.count.get(), 0);

        inner.ready.set(true);
        inner.waker.wake();
        srv.shutdown().await;

        assert_eq!(inner.count.get(), 2);
        assert_eq!(inner.active_on_shutdown.get(), Some(0));
    }

    #[test]
    fn request_canceled_display() {
        let err = BufferServiceError::<&str>::RequestCanceled;
        assert_eq!(err.to_string(), "buffer service request canceled");
    }

    #[ntex::test]
    async fn dropped_requests_are_skipped() {
        let inner = Inner::new(false);
        let srv = Pipeline::new(
            (),
            BufferService::new(1, PipelineState::new(TestService(inner.clone()))),
        );
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        // the first request waits for inner readiness, the second is buffered
        let mut fut1 = srv.call_static(());
        assert!(lazy(|cx| Pin::new(&mut fut1).poll(cx)).await.is_pending());
        let mut fut2 = srv.call_static(());
        assert!(lazy(|cx| Pin::new(&mut fut2).poll(cx)).await.is_pending());
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Pending);

        // dropping a buffered request frees its slot
        drop(fut2);
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));
        let r3 = spawn_call(&srv);
        settle().await;

        // dropping the waiting request releases the next one
        drop(fut1);
        inner.set_ready();
        settle().await;
        assert!(matches!(*r3.borrow(), Some(Ok(()))));
        assert_eq!(inner.count.get(), 1);

        // a released request that is dropped before it runs releases the next one
        let mut fut4 = srv.call_static(());
        assert!(lazy(|cx| Pin::new(&mut fut4).poll(cx)).await.is_pending());
        let mut fut5 = srv.call_static(());
        assert!(lazy(|cx| Pin::new(&mut fut5).poll(cx)).await.is_pending());
        drop(fut4);
        drop(fut5);
        inner.set_ready();
        crate::time::timeout(Duration::from_secs(1), srv.call(()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inner.count.get(), 2);
        crate::time::timeout(Duration::from_secs(1), srv.shutdown())
            .await
            .unwrap();
    }

    #[ntex::test]
    async fn shutdown_skips_dropped_buffered_request() {
        let inner = Inner::new(false);
        let srv = Pipeline::new(
            (),
            BufferService::new(2, PipelineState::new(TestService(inner.clone()))),
        );
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let mut fut1 = srv.call_static(());
        assert!(lazy(|cx| Pin::new(&mut fut1).poll(cx)).await.is_pending());
        let mut fut2 = srv.call_static(());
        assert!(lazy(|cx| Pin::new(&mut fut2).poll(cx)).await.is_pending());
        drop(fut2);
        drop(fut1);

        crate::time::timeout(Duration::from_secs(1), srv.shutdown())
            .await
            .unwrap();
        assert_eq!(inner.count.get(), 0);
    }

    #[derive(Default)]
    struct FailInner {
        fail: Cell<bool>,
        changed: ReadyNotifier,
    }

    struct FailService(Rc<FailInner>);

    impl Service<(), ()> for FailService {
        type Res = ();
        type Error = ();

        fn bind(&self, ctx: BindCtx<'_>) {
            ctx.subscribe(&self.0.changed);
        }

        async fn ready(&self, _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            if self.0.fail.get() {
                Err(())
            } else {
                future::pending().await
            }
        }

        async fn call(&self, (): (), _: Ctx<'_, Self, ()>) -> Result<(), ()> {
            unreachable!("FailService is never ready")
        }
    }

    #[ntex::test]
    async fn inner_ready_error_is_returned_to_requests() {
        let inner = Rc::new(FailInner::default());
        let srv = Pipeline::new(
            (),
            BufferService::new(2, PipelineState::new(FailService(inner.clone()))),
        );
        assert_eq!(lazy(|cx| srv.poll_ready(cx)).await, Poll::Ready(Ok(())));

        let r1 = spawn_call(&srv);
        let r2 = spawn_call(&srv);
        settle().await;
        assert!(r1.borrow().is_none());

        inner.fail.set(true);
        inner.changed.notify();
        crate::time::timeout(Duration::from_secs(1), srv.shutdown())
            .await
            .unwrap();
        settle().await;

        assert!(matches!(
            *r1.borrow(),
            Some(Err(BufferServiceError::Service(())))
        ));
        assert!(matches!(
            *r2.borrow(),
            Some(Err(BufferServiceError::Service(())))
        ));
    }

    #[ntex::test]
    async fn shared_buffer_capacity_notifies_all_roots() {
        let inner = Inner::new(false);
        let buffer = Rc::new(BufferService::new(
            1,
            PipelineState::new(TestService(inner.clone())),
        ));
        let first = Pipeline::new((), buffer.clone());
        let second = Pipeline::new((), buffer);
        first.ready().await.unwrap();
        second.ready().await.unwrap();
        let mut cx = Context::from_waker(std::task::Waker::noop());

        let mut active = Box::pin(first.call(()));
        assert!(active.as_mut().poll(&mut cx).is_pending());
        let mut queued = Box::pin(second.call(()));
        assert!(queued.as_mut().poll(&mut cx).is_pending());
        assert!(first.poll_ready(&mut cx).is_pending());
        assert!(second.poll_ready(&mut cx).is_pending());

        drop(queued);
        assert_eq!(first.poll_ready(&mut cx), Poll::Ready(Ok(())));
        assert_eq!(second.poll_ready(&mut cx), Poll::Ready(Ok(())));
        inner.set_ready();
        assert_eq!(active.as_mut().poll(&mut cx), Poll::Ready(Ok(())));
    }

    #[ntex::test]
    async fn notification_panics_do_not_strand_buffered_requests() {
        let inner = Inner::new(false);
        let buffer = Rc::new(BufferService::new(
            1,
            PipelineState::new(TestService(inner.clone())),
        ));
        let srv = Pipeline::new((), buffer.clone());
        let panic_once = Rc::new(Cell::new(false));
        let flag = panic_once.clone();
        srv.on_notify(&ReadyCallback::new(move || {
            assert!(!flag.replace(false), "notification panic");
        }));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut active = Box::pin(srv.call(()));
        assert!(active.as_mut().poll(&mut cx).is_pending());

        panic_once.set(true);
        let mut queued = Box::pin(srv.call(()));
        assert!(catch_unwind(AssertUnwindSafe(|| queued.as_mut().poll(&mut cx))).is_err());
        drop(queued);
        assert!(buffer.shared.buf.borrow().is_empty());
        assert_eq!(srv.poll_ready(&mut cx), Poll::Ready(Ok(())));

        let mut queued = Box::pin(srv.call(()));
        assert!(queued.as_mut().poll(&mut cx).is_pending());
        panic_once.set(true);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(active))).is_err());
        assert_eq!(
            queued.as_mut().poll(&mut cx),
            Poll::Ready(Err(BufferServiceError::RequestCanceled))
        );
        assert!(buffer.shared.is_drained());

        inner.set_ready();
        assert_eq!(srv.call(()).await, Ok(()));
    }

    #[ntex::test]
    async fn shutdown_notification_panic_still_cancels_waiters() {
        let inner = Inner::new(false);
        let buffer = Rc::new(
            BufferService::new(1, PipelineState::new(TestService(inner))).cancel_on_shutdown(),
        );
        let srv = Pipeline::new((), buffer.clone());
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut active = Box::pin(srv.call(()));
        assert!(active.as_mut().poll(&mut cx).is_pending());
        let mut queued = Box::pin(srv.call(()));
        assert!(queued.as_mut().poll(&mut cx).is_pending());
        srv.on_notify(&ReadyCallback::new(|| panic!("shutdown notification")));

        assert!(catch_unwind(AssertUnwindSafe(|| srv.poll_shutdown(&mut cx))).is_err());
        assert_eq!(
            active.as_mut().poll(&mut cx),
            Poll::Ready(Err(BufferServiceError::RequestCanceled))
        );
        assert_eq!(
            queued.as_mut().poll(&mut cx),
            Poll::Ready(Err(BufferServiceError::RequestCanceled))
        );
        assert!(buffer.shared.is_drained());
    }
}
