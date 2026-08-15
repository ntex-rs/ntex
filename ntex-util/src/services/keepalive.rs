#![allow(dead_code)]
use std::{cell::Cell, convert::Infallible, fmt, marker, time};

use ntex_service::{Ctx, ReadyCtx, Service, ServiceFactory};

use crate::time::{Millis, Sleep, now, sleep};

/// `KeepAlive` service factory
///
/// Controls min time between requests.
pub struct KeepAlive<F, St, Req, E, C>
where
    F: Fn() -> E + Clone,
{
    f: F,
    ka: Millis,
    _t: marker::PhantomData<(E, St, Req, C)>,
}

impl<F, St, Req, E, C> KeepAlive<F, St, Req, E, C>
where
    F: Fn() -> E + Clone,
{
    /// Construct `KeepAlive` service factory.
    ///
    /// ka - keep-alive timeout
    /// err - error factory function
    pub fn new(ka: Millis, f: F) -> Self {
        KeepAlive {
            f,
            ka,
            _t: marker::PhantomData,
        }
    }
}

impl<F, St, Req, E, C> Clone for KeepAlive<F, St, Req, E, C>
where
    F: Fn() -> E + Clone,
{
    fn clone(&self) -> Self {
        KeepAlive {
            f: self.f.clone(),
            ka: self.ka,
            _t: marker::PhantomData,
        }
    }
}

impl<F, St, Req, E, C> fmt::Debug for KeepAlive<F, St, Req, E, C>
where
    F: Fn() -> E + Clone,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAlive")
            .field("ka", &self.ka)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<F, St, Req, E, C> ServiceFactory<Req> for KeepAlive<F, St, Req, E, C>
where
    F: Fn() -> E + Clone,
{
    type St = St;
    type Res = Req;
    type Error = E;

    type Service = KeepAliveService<St, Req, E, F>;
    type InitCfg = C;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &C) -> Result<Self::Service, Self::InitError> {
        Ok(KeepAliveService::new(self.ka, self.f.clone()))
    }
}

pub struct KeepAliveService<St, Req, E, F>
where
    F: Fn() -> E,
{
    f: F,
    dur: Millis,
    sleep: Sleep,
    expire: Cell<time::Instant>,
    _t: marker::PhantomData<(St, Req, E)>,
}

impl<St, Req, E, F> KeepAliveService<St, Req, E, F>
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

impl<St, Req, E, F> fmt::Debug for KeepAliveService<St, Req, E, F>
where
    F: Fn() -> E,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAliveService")
            .field("dur", &self.dur)
            .field("expire", &self.expire)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<St, Req, E, F> Service for KeepAliveService<St, Req, E, F>
where
    F: Fn() -> E,
{
    type St = St;
    type Req = Req;
    type Res = Req;
    type Error = E;

    async fn ready(&self, _: ReadyCtx<'_, Self>) -> Result<(), Self::Error> {
        let expire = self.expire.get() + time::Duration::from(self.dur);
        if expire <= now() { Err((self.f)()) } else { Ok(()) }
    }

    // fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
    //     match self.sleep.poll_elapsed(cx) {
    //         Poll::Ready(()) => {
    //             let now = now();
    //             let expire = self.expire.get() + time::Duration::from(self.dur);
    //             if expire <= now {
    //                 Err((self.f)())
    //             } else {
    //                 let expire = expire - now;
    //                 self.sleep
    //                     .reset(Millis(expire.as_millis().try_into().unwrap_or(u32::MAX)));
    //                 let _ = self.sleep.poll_elapsed(cx);
    //                 Ok(())
    //             }
    //         }
    //         Poll::Pending => Ok(()),
    //     }
    // }

    #[inline]
    async fn call(&self, req: Req, _: Ctx<'_, Self>) -> Result<Req, E> {
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
        let factory = KeepAlive::<_, (), usize, _, _>::new(Millis(100), || TestErr);
        assert!(format!("{factory:?}").contains("KeepAlive"));
        let _ = factory.clone();

        let service = factory.pipeline(&()).await.unwrap();
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
