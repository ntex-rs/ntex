use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use std::{cell::Cell, convert::Infallible, marker};

use crate::time::{sleep_duration, Sleep};
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
    dur: Duration,
    time: LowResTimeService,
    sleep: Sleep,
    expire: Cell<Instant>,
    _t: marker::PhantomData<(R, E)>,
}

impl<R, E, F> KeepAliveService<R, E, F>
where
    F: Fn() -> E,
{
    pub fn new(dur: Duration, time: LowResTimeService, f: F) -> Self {
        let expire = Cell::new(time.now() + dur);

        KeepAliveService {
            f,
            dur,
            time,
            expire,
            sleep: sleep_duration(dur),
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
        match self.sleep.poll_elapsed(cx) {
            Poll::Ready(_) => {
                let now = self.time.now();
                if self.expire.get() <= now {
                    Poll::Ready(Err((self.f)()))
                } else {
                    let expire = self.expire.get() - Instant::now();
                    self.sleep.reset(expire.as_millis() as u64);
                    let _ = self.sleep.poll_elapsed(cx);
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Pending => Poll::Ready(Ok(())),
        }
    }

    fn call(&self, req: R) -> Self::Future {
        self.expire.set(self.time.now() + self.dur);
        Ready::Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{Service, ServiceFactory};
    use crate::time::sleep;
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

        sleep(500).await;
        assert_eq!(
            lazy(|cx| service.poll_ready(cx)).await,
            Poll::Ready(Err(TestErr))
        );
    }
}
