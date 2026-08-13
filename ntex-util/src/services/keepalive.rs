use std::future::Future;
use std::{cell::Cell, convert::Infallible, fmt, marker, task::Context, task::Poll, time};

use ntex_service::{Ctx, ReadyCtx, Service, ServiceFactory};

use crate::future::Ready;
use crate::time::{Millis, Sleep, now, sleep};

/// `KeepAlive` service factory
///
/// Controls min time between requests.
pub struct KeepAlive<E, F, C> {
    f: F,
    ka: Millis,
    _t: marker::PhantomData<(E, C)>,
}

impl<E, F, C> KeepAlive<E, F, C>
where
    F: Fn() -> E + Clone,
{
    /// Construct `KeepAlive` service factory.
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

impl<E, F, C> Clone for KeepAlive<E, F, C>
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

impl<E, F, C> fmt::Debug for KeepAlive<E, F, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAlive")
            .field("ka", &self.ka)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<St, Req, E, F, C> ServiceFactory<St, Req> for KeepAlive<E, F, C>
where
    F: Fn() -> E + Clone,
{
    type Res = Req;
    type Error = E;

    type Service = KeepAliveService<E, F>;
    type InitCfg = C;
    type InitError = Infallible;

    #[inline]
    fn create(
        &self,
        _: &C,
    ) -> impl Future<Output = Result<Self::Service, Self::InitError>> {
        Ready::Ok(KeepAliveService::new(self.ka, self.f.clone()))
    }
}

pub struct KeepAliveService<E, F> {
    f: F,
    dur: Millis,
    sleep: Sleep,
    expire: Cell<time::Instant>,
    _t: marker::PhantomData<E>,
}

impl<E, F> KeepAliveService<E, F>
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

impl<E, F> fmt::Debug for KeepAliveService<E, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAliveService")
            .field("dur", &self.dur)
            .field("expire", &self.expire)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<St, Req, E, F> Service<St, Req> for KeepAliveService<E, F>
where
    F: Fn() -> E,
{
    type Res = Req;
    type Error = E;

    async fn ready(&self, _: ReadyCtx<'_, Self, St>) -> Result<(), Self::Error> {
        let expire = self.expire.get() + time::Duration::from(self.dur);
        if expire <= now() { Err((self.f)()) } else { Ok(()) }
    }

    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        match self.sleep.poll_elapsed(cx) {
            Poll::Ready(()) => {
                let now = now();
                let expire = self.expire.get() + time::Duration::from(self.dur);
                if expire <= now {
                    Err((self.f)())
                } else {
                    let expire = expire - now;
                    self.sleep
                        .reset(Millis(expire.as_millis().try_into().unwrap_or(u32::MAX)));
                    let _ = self.sleep.poll_elapsed(cx);
                    Ok(())
                }
            }
            Poll::Pending => Ok(()),
        }
    }

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self, St>) -> Result<Req, E> {
        self.expire.set(now());
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use super::*;
    use crate::future::lazy;

    #[derive(Debug, PartialEq)]
    struct TestErr;

    #[ntex::test]
    async fn test_ka() {
        let factory = KeepAlive::new(Millis(100), || TestErr);
        assert!(format!("{factory:?}").contains("KeepAlive"));
        let _ = factory.clone();

        let service = factory.pipeline(&()).await.unwrap().bind();
        assert!(format!("{service:?}").contains("KeepAliveService"));

        assert_eq!(service.call(1usize).await, Ok(1usize));
        assert!(lazy(|cx| service.poll_ready(cx)).await.is_ready());

        sleep(Millis(500)).await;
        assert_eq!(
            lazy(|cx| service.poll_ready(cx)).await,
            Poll::Ready(Err(TestErr))
        );
    }
}
