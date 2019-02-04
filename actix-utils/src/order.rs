use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use actix_service::{NewTransform, Service, Transform};
use futures::future::{ok, FutureResult};
use futures::task::AtomicTask;
use futures::unsync::oneshot;
use futures::{Async, Future, Poll};

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

    pub fn service() -> impl Transform<
        S,
        Request = S::Request,
        Response = S::Response,
        Error = InOrderError<S::Error>,
    > {
        InOrderService::new()
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

impl<S> NewTransform<S> for InOrder<S>
where
    S: Service,
    S::Response: 'static,
    S::Future: 'static,
    S::Error: 'static,
{
    type Request = S::Request;
    type Response = S::Response;
    type Error = InOrderError<S::Error>;
    type InitError = ();
    type Transform = InOrderService<S>;
    type Future = FutureResult<Self::Transform, Self::InitError>;

    fn new_transform(&self) -> Self::Future {
        ok(InOrderService::new())
    }
}

pub struct InOrderService<S: Service> {
    task: Rc<AtomicTask>,
    acks: VecDeque<Record<S::Response, S::Error>>,
}

impl<S> InOrderService<S>
where
    S: Service,
    S::Response: 'static,
    S::Future: 'static,
    S::Error: 'static,
{
    pub fn new() -> Self {
        Self {
            acks: VecDeque::new(),
            task: Rc::new(AtomicTask::new()),
        }
    }
}

impl<S> Default for InOrderService<S>
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

impl<S> Transform<S> for InOrderService<S>
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

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.task.register();

        // check acks
        while !self.acks.is_empty() {
            let rec = self.acks.front_mut().unwrap();
            match rec.rx.poll() {
                Ok(Async::Ready(res)) => {
                    let rec = self.acks.pop_front().unwrap();
                    let _ = rec.tx.send(res);
                }
                Ok(Async::NotReady) => break,
                Err(oneshot::Canceled) => return Err(InOrderError::Disconnected),
            }
        }

        Ok(Async::Ready(()))
    }

    fn call(&mut self, request: S::Request, service: &mut S) -> Self::Future {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        self.acks.push_back(Record { rx: rx1, tx: tx2 });

        let task = self.task.clone();
        tokio_current_thread::spawn(service.call(request).then(move |res| {
            task.notify();
            let _ = tx1.send(res);
            Ok(())
        }));

        InOrderServiceResponse { rx: rx2 }
    }
}

#[doc(hidden)]
pub struct InOrderServiceResponse<S: Service> {
    rx: oneshot::Receiver<Result<S::Response, S::Error>>,
}

impl<S: Service> Future for InOrderServiceResponse<S> {
    type Item = S::Response;
    type Error = InOrderError<S::Error>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.rx.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(Ok(res))) => Ok(Async::Ready(res)),
            Ok(Async::Ready(Err(e))) => Err(e.into()),
            Err(oneshot::Canceled) => Err(InOrderError::Disconnected),
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::future::{lazy, Future};
    use futures::{stream::futures_unordered, sync::oneshot, Async, Poll, Stream};

    use std::time::Duration;

    use super::*;
    use actix_service::{Blank, Service, ServiceExt};

    struct Srv;

    impl Service for Srv {
        type Request = oneshot::Receiver<usize>;
        type Response = usize;
        type Error = ();
        type Future = Box<Future<Item = usize, Error = ()>>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(Async::Ready(()))
        }

        fn call(&mut self, req: oneshot::Receiver<usize>) -> Self::Future {
            Box::new(req.map_err(|_| ()))
        }
    }

    struct SrvPoll<S: Service> {
        s: S,
    }

    impl<S: Service> Future for SrvPoll<S> {
        type Item = ();
        type Error = ();

        fn poll(&mut self) -> Poll<(), ()> {
            let _ = self.s.poll_ready();
            Ok(Async::NotReady)
        }
    }

    #[test]
    fn test_inorder() {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        let (tx3, rx3) = oneshot::channel();
        let (tx_stop, rx_stop) = oneshot::channel();

        std::thread::spawn(move || {
            let rx1 = rx1;
            let rx2 = rx2;
            let rx3 = rx3;
            let tx_stop = tx_stop;
            let _ = actix_rt::System::new("test").block_on(lazy(move || {
                let mut srv = Blank::new().apply(InOrderService::new(), Srv);

                let res1 = srv.call(rx1);
                let res2 = srv.call(rx2);
                let res3 = srv.call(rx3);
                tokio_current_thread::spawn(SrvPoll { s: srv });

                futures_unordered(vec![res1, res2, res3])
                    .collect()
                    .and_then(move |res: Vec<_>| {
                        assert_eq!(res, vec![1, 2, 3]);
                        let _ = tx_stop.send(());
                        Ok(())
                    })
            }));
        });

        let _ = tx3.send(3);
        std::thread::sleep(Duration::from_millis(50));
        let _ = tx2.send(2);
        let _ = tx1.send(1);

        let _ = rx_stop.wait();
    }
}
