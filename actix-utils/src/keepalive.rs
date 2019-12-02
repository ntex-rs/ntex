use std::convert::Infallible;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use actix_rt::time::{delay, Delay};
use actix_service::{Service, ServiceFactory};
use futures::future::{ok, Ready};

use super::time::{LowResTime, LowResTimeService};

pub struct KeepAlive<R, E, F> {
    f: F,
    ka: Duration,
    time: LowResTime,
    _t: PhantomData<(R, E)>,
}

impl<R, E, F> KeepAlive<R, E, F>
where
    F: Fn() -> E + Clone,
{
    pub fn new(ka: Duration, time: LowResTime, f: F) -> Self {
        KeepAlive {
            f,
            ka,
            time,
            _t: PhantomData,
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
            _t: PhantomData,
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
    type Future = Ready<Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        ok(KeepAliveService::new(
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
    delay: Delay,
    expire: Instant,
    _t: PhantomData<(R, E)>,
}

impl<R, E, F> KeepAliveService<R, E, F>
where
    F: Fn() -> E,
{
    pub fn new(ka: Duration, time: LowResTimeService, f: F) -> Self {
        let expire = time.now() + ka;
        KeepAliveService {
            f,
            ka,
            time,
            expire,
            delay: delay(expire),
            _t: PhantomData,
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
    type Future = Ready<Result<R, E>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match Pin::new(&mut self.delay).poll(cx) {
            Poll::Ready(_) => {
                let now = self.time.now();
                if self.expire <= now {
                    Poll::Ready(Err((self.f)()))
                } else {
                    self.delay.reset(self.expire);
                    let _ = Pin::new(&mut self.delay).poll(cx);
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Pending => Poll::Ready(Ok(())),
        }
    }

    fn call(&mut self, req: R) -> Self::Future {
        self.expire = self.time.now() + self.ka;
        ok(req)
    }
}
