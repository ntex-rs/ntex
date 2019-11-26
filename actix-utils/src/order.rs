use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use actix_service::{IntoService, Service, Transform};
use futures::future::{ok, Ready};

use crate::oneshot;
use crate::task::LocalWaker;

struct Record<I, E> {
    rx: oneshot::Receiver<Result<I, E>>,
    tx: oneshot::Sender<Result<I, E>>,
}

/// Timeout error
pub enum InOrderError<E> {
    /// Service error
    Service(E),
    /// Service call dropped
    Disconnected,
}

impl<E> From<E> for InOrderError<E> {
    fn from(err: E) -> Self {
        InOrderError::Service(err)
    }
}

impl<E: fmt::Debug> fmt::Debug for InOrderError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            InOrderError::Service(e) => write!(f, "InOrderError::Service({:?})", e),
            InOrderError::Disconnected => write!(f, "InOrderError::Disconnected"),
        }
    }
}

impl<E: fmt::Display> fmt::Display for InOrderError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            InOrderError::Service(e) => e.fmt(f),
            InOrderError::Disconnected => write!(f, "InOrder service disconnected"),
        }
    }
}

/// InOrder - The service will yield responses as they become available,
/// in the order that their originating requests were submitted to the service.
pub struct InOrder<S> {
    _t: PhantomData<S>,
}

impl<S> InOrder<S>
where
    S: Service,
    S::Response: 'static,
    S::Future: 'static,
    S::Error: 'static,
{
    pub fn new() -> Self {
        Self { _t: PhantomData }
    }

    pub fn service(service: S) -> InOrderService<S> {
        InOrderService::new(service)
    }
}

impl<S> Default for InOrder<S>
where
    S: Service,
    S::Response: 'static,
    S::Future: 'static,
    S::Error: 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Transform<S> for InOrder<S>
where
    S: Service,
    S::Response: 'static,
    S::Future: 'static,
    S::Error: 'static,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = InOrderError<S::Error>;
    type InitError = Infallible;
    type Transform = InOrderService<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(InOrderService::new(service))
    }
}

pub struct InOrderService<S: Service> {
    service: S,
    task: Rc<LocalWaker>,
    acks: VecDeque<Record<S::Response, S::Error>>,
}

impl<S> InOrderService<S>
where
    S: Service,
    S::Response: 'static,
    S::Future: 'static,
    S::Error: 'static,
{
    pub fn new<U>(service: U) -> Self
    where
        U: IntoService<S>,
    {
        Self {
            service: service.into_service(),
            acks: VecDeque::new(),
            task: Rc::new(LocalWaker::new()),
        }
    }
}

impl<S> Service for InOrderService<S>
where
    S: Service,
    S::Response: 'static,
    S::Future: 'static,
    S::Error: 'static,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = InOrderError<S::Error>;
    type Future = InOrderServiceResponse<S>;

    fn poll_ready(&mut self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        // poll_ready could be called from different task
        self.task.register(cx.waker());

        // check acks
        while !self.acks.is_empty() {
            let rec = self.acks.front_mut().unwrap();
            match Pin::new(&mut rec.rx).poll(cx) {
                Poll::Ready(Ok(res)) => {
                    let rec = self.acks.pop_front().unwrap();
                    let _ = rec.tx.send(res);
                }
                Poll::Pending => break,
                Poll::Ready(Err(oneshot::Canceled)) => {
                    return Poll::Ready(Err(InOrderError::Disconnected))
                }
            }
        }

        // check nested service
        if let Poll::Pending = self.service.poll_ready(cx).map_err(InOrderError::Service)? {
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn call(&mut self, request: S::Request) -> Self::Future {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        self.acks.push_back(Record { rx: rx1, tx: tx2 });

        let task = self.task.clone();
        let fut = self.service.call(request);
        actix_rt::spawn(async move {
            let res = fut.await;
            task.wake();
            let _ = tx1.send(res);
        });

        InOrderServiceResponse { rx: rx2 }
    }
}

#[doc(hidden)]
pub struct InOrderServiceResponse<S: Service> {
    rx: oneshot::Receiver<Result<S::Response, S::Error>>,
}

impl<S: Service> Future for InOrderServiceResponse<S> {
    type Output = Result<S::Response, InOrderError<S::Error>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(Ok(res))) => Poll::Ready(Ok(res)),
            Poll::Ready(Ok(Err(e))) => Poll::Ready(Err(e.into())),
            Poll::Ready(Err(_)) => Poll::Ready(Err(InOrderError::Disconnected)),
        }
    }
}

#[cfg(test)]
mod tests {

    use std::task::{Context, Poll};
    use std::time::Duration;

    use super::*;
    use actix_service::Service;
    use futures::channel::oneshot;
    use futures::future::{lazy, FutureExt, LocalBoxFuture};

    struct Srv;

    impl Service for Srv {
        type Request = oneshot::Receiver<usize>;
        type Response = usize;
        type Error = ();
        type Future = LocalBoxFuture<'static, Result<usize, ()>>;

        fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: oneshot::Receiver<usize>) -> Self::Future {
            req.map(|res| res.map_err(|_| ())).boxed_local()
        }
    }

    #[actix_rt::test]
    async fn test_inorder() {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        let (tx3, rx3) = oneshot::channel();
        let (tx_stop, rx_stop) = oneshot::channel();

        let h = std::thread::spawn(move || {
            let rx1 = rx1;
            let rx2 = rx2;
            let rx3 = rx3;
            let tx_stop = tx_stop;
            let _ = actix_rt::System::new("test").block_on(async {
                let mut srv = InOrderService::new(Srv);

                let res1 = srv.call(rx1);
                let res2 = srv.call(rx2);
                let res3 = srv.call(rx3);

                let _ = lazy(|cx| srv.poll_ready(cx)).await;

                // dispatcher do this
                actix_rt::time::delay_for(Duration::from_millis(100)).await;
                let _ = lazy(|cx| srv.poll_ready(cx)).await;

                assert_eq!(res1.await.unwrap(), 1);
                assert_eq!(res2.await.unwrap(), 2);
                assert_eq!(res3.await.unwrap(), 3);

                let _ = tx_stop.send(());
                actix_rt::System::current().stop();
            });
        });

        let _ = tx3.send(3);
        std::thread::sleep(Duration::from_millis(50));
        let _ = tx2.send(2);
        let _ = tx1.send(1);

        let _ = rx_stop.await;
        let _ = h.join();
    }
}
