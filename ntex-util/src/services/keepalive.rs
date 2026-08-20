use std::{cell::Cell, convert::Infallible, fmt, task::Poll, time};

use ntex_service::{Ctx, Service, ServiceFactory};

use crate::time::{Millis, Sleep, now, sleep};

/// `KeepAlive` service factory
///
/// Controls min time between requests.
pub struct KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    f: F,
    ka: Millis,
}

impl<F, E> KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    /// Construct `KeepAlive` service factory.
    ///
    /// ka - keep-alive timeout
    /// err - error factory function
    pub fn new(ka: Millis, f: F) -> Self {
        KeepAlive { f, ka }
    }
}

impl<F, E> Clone for KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    fn clone(&self) -> Self {
        KeepAlive {
            f: self.f.clone(),
            ka: self.ka,
        }
    }
}

impl<F, E> fmt::Debug for KeepAlive<F, E>
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

impl<F, E, St, Req, Cfg> ServiceFactory<St, Req, Cfg> for KeepAlive<F, E>
where
    F: Fn() -> E + Clone,
{
    type Res = Req;
    type Error = E;

    type Service = KeepAliveService<F, E>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: &Cfg) -> Result<Self::Service, Self::InitError> {
        Ok(KeepAliveService::new(self.ka, self.f.clone()))
    }
}

pub struct KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    f: F,
    dur: time::Duration,
    sleep: Sleep,
    expire: Cell<time::Instant>,
}

impl<F, E> KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    pub fn new(dur: Millis, f: F) -> Self {
        let expire = Cell::new(now());

        KeepAliveService {
            f,
            expire,
            sleep: sleep(dur),
            dur: time::Duration::from(dur),
        }
    }
}

impl<F, E> fmt::Debug for KeepAliveService<F, E>
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

impl<F, E, St, Req> Service<St, Req> for KeepAliveService<F, E>
where
    F: Fn() -> E,
{
    type Res = Req;
    type Error = E;

    async fn ready(&self, ctx: Ctx<'_, Self, St>) -> Result<(), Self::Error> {
        let expire = self.expire.get() + self.dur;
        if expire <= now() {
            Err((self.f)())
        } else {
            ctx.poll_once(|cx| match self.sleep.poll_elapsed(cx) {
                Poll::Ready(()) => {
                    let now = now();
                    let expire = self.expire.get() + self.dur;
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
            })
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
    use std::{pin::Pin, task::Context, task::Poll, task::ready};

    use ntex_service::{Pipeline, boxed, factory};

    use super::*;
    use crate::{channel::oneshot, spawn};

    #[derive(Debug, PartialEq)]
    struct TestErr;

    struct Dispatcher {
        p: Pipeline<usize, usize, TestErr>,
        tx: Option<oneshot::Sender<()>>,
    }

    impl Future for Dispatcher {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut this = self.as_mut();
            if ready!(this.p.poll_ready(cx)).is_err() {
                if let Some(tx) = this.tx.take() {
                    let _ = tx.send(());
                }
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    #[ntex::test]
    async fn test_ka() {
        let factory = factory::<_, usize, ()>(KeepAlive::new(Millis(100), || TestErr));
        assert!(format!("{factory:?}").contains("KeepAlive"));
        let _ = factory.clone();

        let svc = factory.create(&()).await.unwrap();
        assert!(format!("{svc:?}").contains("KeepAliveService"));

        let p = Pipeline::new(boxed::service(svc));
        assert_eq!(p.call(1usize).await, Ok(1usize));
        let svc = p.bind();

        let (tx, rx) = oneshot::channel();
        spawn(Dispatcher { p, tx: Some(tx) }).detach();

        sleep(Millis(25)).await;
        assert_eq!(svc.call(1usize).await, Ok(1usize));
        sleep(Millis(100)).await;

        let res = rx.await;
        assert_eq!(res, Ok(()));
        assert_eq!(svc.ready().await, Err(TestErr));
    }
}
