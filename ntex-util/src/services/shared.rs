/// A service that can be checked for readiness by multiple tasks
use std::{
    cell::Cell, cell::RefCell, marker::PhantomData, rc::Rc, task::Context, task::Poll,
};

use ntex_service::{Middleware, Service};

use crate::channel::{condition, oneshot};
use crate::future::{poll_fn, select, Either};

/// A middleware that construct sharable service
pub struct Shared<R>(PhantomData<R>);

impl<R> Shared<R> {
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<R> Default for Shared<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Service<R>, R> Middleware<S> for Shared<R> {
    type Service = SharedService<S, R>;

    fn create(&self, service: S) -> Self::Service {
        SharedService::new(service)
    }
}

/// A service that can be checked for readiness by multiple tasks
pub struct SharedService<S: Service<R>, R> {
    inner: Rc<Inner<S, R>>,
    readiness: condition::Waiter,
}

struct Inner<S: Service<R>, R> {
    service: S,
    ready: condition::Condition,
    driver_stop: Cell<Option<oneshot::Sender<()>>>,
    driver_running: Cell<bool>,
    error: RefCell<Option<S::Error>>,
}

impl<S: Service<R>, R> SharedService<S, R> {
    pub fn new(service: S) -> Self {
        let condition = condition::Condition::default();
        Self {
            readiness: condition.wait(),
            inner: Rc::new(Inner {
                service,
                ready: condition,
                driver_stop: Cell::default(),
                driver_running: Cell::default(),
                error: RefCell::default(),
            }),
        }
    }
}

impl<S: Service<R>, R> Clone for SharedService<S, R> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            readiness: self.readiness.clone(),
        }
    }
}

impl<S: Service<R>, R> Drop for SharedService<S, R> {
    fn drop(&mut self) {
        if self.inner.driver_running.get() {
            // the only live references to inner are in this SharedService instance and the driver task
            if Rc::strong_count(&self.inner) == 2 {
                if let Some(stop) = self.inner.driver_stop.take() {
                    let _ = stop.send(());
                }
            }
        }
    }
}

impl<S, R> Service<R> for SharedService<S, R>
where
    S: Service<R> + 'static,
    S::Error: Clone,
    R: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future<'f> = S::Future<'f> where Self: 'f, R: 'f;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // if there is an error, it should be returned to all tasks checking readiness
        if let Some(error) = self.inner.error.borrow().as_ref() {
            return Poll::Ready(Err(error.clone()));
        }

        // if the service is being driven to readiness we must register our waker and wait
        if self.inner.driver_running.get() {
            log::trace!("polled SharedService driver, driver is already running");
            // register waker to be notified, regardless of any previous notification
            let _ = self.readiness.poll_ready(cx);
            return Poll::Pending;
        }

        // driver is not running, check the inner service is ready
        let result = self.inner.service.poll_ready(cx);
        log::trace!(
            "polled SharedService, ready: {}, errored: {}",
            result.is_ready(),
            matches!(result, Poll::Ready(Err(_)))
        );

        match result {
            // pass through service is ready, allow call
            Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
            // capture error, all future readiness checks will fail
            Poll::Ready(Err(e)) => {
                *self.inner.error.borrow_mut() = Some(e.clone());
                Poll::Ready(Err(e))
            }
            // start driver and elide all poll_ready calls until it is complete
            Poll::Pending => {
                let inner = self.inner.clone();
                let (tx, rx) = oneshot::channel();
                inner.driver_running.set(true);
                inner.driver_stop.set(Some(tx));

                ntex_rt::spawn(async move {
                    log::trace!("SharedService driver has started");
                    let service_ready = poll_fn(|cx| inner.service.poll_ready(cx));
                    let clients_gone = rx;
                    let result = select(service_ready, clients_gone).await;
                    if let Either::Left(result) = result {
                        log::trace!(
                            "SharedService driver completed, errored: {}",
                            result.is_err()
                        );
                        if let Err(e) = result {
                            inner.error.borrow_mut().replace(e);
                        }
                        inner.driver_running.set(false);
                        inner.driver_stop.set(None);
                        inner.ready.notify();
                    } else {
                        log::trace!("SharedService driver task stopped because all clients are gone");
                    }
                });

                // register waker to be notified, regardless of any previous notification
                let _ = self.readiness.poll_ready(cx);

                Poll::Pending
            }
        }
    }

    fn poll_shutdown(&self, cx: &mut Context<'_>) -> Poll<()> {
        self.inner.service.poll_shutdown(cx)
    }

    fn call(&self, req: R) -> Self::Future<'_> {
        self.inner.service.call(req)
    }
}
