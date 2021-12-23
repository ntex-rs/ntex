use std::task::{Context, Poll};
use std::{cell::Cell, convert::Infallible, marker, time::Duration, time::Instant};

use crate::time::{now, sleep, Millis, Sleep};
use crate::{util::Ready, Service, ServiceFactory};

/// KeepAlive service factory
///
/// Controls min time between requests.
pub struct KeepAlive<R, E, F> {
    f: F,
    ka: Millis,
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
    pub fn new(ka: Millis, err: F) -> Self {
        KeepAlive {
            ka,
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
            _t: marker::PhantomData,
        }
    }
}

impl<R, E, F, C> ServiceFactory<R, C> for KeepAlive<R, E, F>
where
    F: Fn() -> E + Clone,
{
    type Response = R;
    type Error = E;
    type InitError = Infallible;
    type Service = KeepAliveService<R, E, F>;
    type Future = Ready<Self::Service, Self::InitError>;

    #[inline]
    fn new_service(&self, _: C) -> Self::Future {
        Ready::Ok(KeepAliveService::new(self.ka, self.f.clone()))
    }
}

pub struct KeepAliveService<R, E, F> {
    f: F,
    dur: Millis,
    sleep: Sleep,
    expire: Cell<Instant>,
    _t: marker::PhantomData<(R, E)>,
}

impl<R, E, F> KeepAliveService<R, E, F>
where
    F: Fn() -> E,
{
    pub fn new(dur: Millis, f: F) -> Self {
        let expire = Cell::new(now());

        KeepAliveService {
            f,
            dur,
            expire,
            sleep: sleep(dur),
            _t: marker::PhantomData,
        }
    }
}

impl<R, E, F> Service<R> for KeepAliveService<R, E, F>
where
    F: Fn() -> E,
{
    type Response = R;
    type Error = E;
    type Future = Ready<R, E>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.sleep.poll_elapsed(cx) {
            Poll::Ready(_) => {
                let now = now();
                let expire = self.expire.get() + Duration::from(self.dur);
                if expire <= now {
                    Poll::Ready(Err((self.f)()))
                } else {
                    let expire = expire - now;
                    self.sleep.reset(Millis(expire.as_millis() as u64));
                    let _ = self.sleep.poll_elapsed(cx);
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Pending => Poll::Ready(Ok(())),
        }
    }

    fn call(&self, req: R) -> Self::Future {
        self.expire.set(now());
        Ready::Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Service, ServiceFactory};
    use crate::util::lazy;

    #[derive(Debug, PartialEq)]
    struct TestErr;

    #[crate::rt_test]
    async fn test_ka() {
        let factory = KeepAlive::new(Millis(100), || TestErr);
        let _ = factory.clone();

        let service = factory.new_service(()).await.unwrap();

        assert_eq!(service.call(1usize).await, Ok(1usize));
        assert!(lazy(|cx| service.poll_ready(cx)).await.is_ready());

        sleep(Millis(500)).await;
        assert_eq!(
            lazy(|cx| service.poll_ready(cx)).await,
            Poll::Ready(Err(TestErr))
        );
    }
}
