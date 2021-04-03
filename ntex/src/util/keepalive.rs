use std::task::{Context, Poll};
use std::{
    cell::RefCell, convert::Infallible, future::Future, marker, pin::Pin, time::Duration,
};

use crate::rt::time::{sleep_until, Instant, Sleep};
use crate::{util::Ready, Service, ServiceFactory};

use super::time::{LowResTime, LowResTimeService};

/// KeepAlive service factory
///
/// Controls min time between requests.
pub struct KeepAlive<R, E, F> {
    f: F,
    ka: Duration,
    time: LowResTime,
    _t: marker::PhantomData<(R, E)>,
}

impl<R, E, F> KeepAlive<R, E, F>
where
    F: Fn() -> E + Clone,
{
    /// Construct KeepAlive service factory.
    ///
    /// ka - keep-alive timeout
    /// err - error factory function
    pub fn new(ka: Duration, time: LowResTime, err: F) -> Self {
        KeepAlive {
            ka,
            time,
            f: err,
            _t: marker::PhantomData,
        }
    }
}

impl<R, E, F> Clone for KeepAlive<R, E, F>
where
    F: Clone,
{
    fn clone(&self) -> Self {
        KeepAlive {
            f: self.f.clone(),
            ka: self.ka,
            time: self.time.clone(),
            _t: marker::PhantomData,
        }
    }
}

impl<R, E, F> ServiceFactory for KeepAlive<R, E, F>
where
    F: Fn() -> E + Clone,
{
    type Request = R;
    type Response = R;
    type Error = E;
    type InitError = Infallible;
    type Config = ();
    type Service = KeepAliveService<R, E, F>;
    type Future = Ready<Self::Service, Self::InitError>;

    fn new_service(&self, _: ()) -> Self::Future {
        Ready::Ok(KeepAliveService::new(
            self.ka,
            self.time.timer(),
            self.f.clone(),
        ))
    }
}

pub struct KeepAliveService<R, E, F> {
    f: F,
    ka: Duration,
    time: LowResTimeService,
    inner: RefCell<Inner>,
    _t: marker::PhantomData<(R, E)>,
}

struct Inner {
    delay: Pin<Box<Sleep>>,
    expire: Instant,
}

impl<R, E, F> KeepAliveService<R, E, F>
where
    F: Fn() -> E,
{
    pub fn new(ka: Duration, time: LowResTimeService, f: F) -> Self {
        let expire = Instant::from_std(time.now() + ka);
        KeepAliveService {
            f,
            ka,
            time,
            inner: RefCell::new(Inner {
                expire,
                delay: Box::pin(sleep_until(expire)),
            }),
            _t: marker::PhantomData,
        }
    }
}

impl<R, E, F> Service for KeepAliveService<R, E, F>
where
    F: Fn() -> E,
{
    type Request = R;
    type Response = R;
    type Error = E;
    type Future = Ready<R, E>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let mut inner = self.inner.borrow_mut();

        match Pin::new(&mut inner.delay).poll(cx) {
            Poll::Ready(_) => {
                let now = Instant::from_std(self.time.now());
                if inner.expire <= now {
                    Poll::Ready(Err((self.f)()))
                } else {
                    let expire = inner.expire;
                    inner.delay.as_mut().reset(expire);
                    let _ = Pin::new(&mut inner.delay).poll(cx);
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Pending => Poll::Ready(Ok(())),
        }
    }

    fn call(&self, req: R) -> Self::Future {
        self.inner.borrow_mut().expire = Instant::from_std(self.time.now() + self.ka);
        Ready::Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rt::time::sleep;
    use crate::service::{Service, ServiceFactory};
    use crate::util::lazy;

    #[derive(Debug, PartialEq)]
    struct TestErr;

    #[crate::rt_test]
    async fn test_ka() {
        let factory = KeepAlive::new(
            Duration::from_millis(100),
            LowResTime::with(Duration::from_millis(10)),
            || TestErr,
        );
        let _ = factory.clone();

        let service = factory.new_service(()).await.unwrap();

        assert_eq!(service.call(1usize).await, Ok(1usize));
        assert!(lazy(|cx| service.poll_ready(cx)).await.is_ready());

        sleep(Duration::from_millis(500)).await;
        assert_eq!(
            lazy(|cx| service.poll_ready(cx)).await,
            Poll::Ready(Err(TestErr))
        );
    }
}
