use std::{cell::Cell, convert::Infallible, fmt, marker, task::Context, task::Poll, time};

use ntex_service::{Service, ServiceCtx, ServiceFactory};

use crate::time::{Millis, Sleep, now, sleep};

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

impl<R, E, F> fmt::Debug for KeepAlive<R, E, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAlive")
            .field("ka", &self.ka)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<R, E, F, C> ServiceFactory<R, C> for KeepAlive<R, E, F>
where
    F: Fn() -> E + Clone,
{
    type Response = R;
    type Error = E;

    type Service = KeepAliveService<R, E, F>;
    type InitError = Infallible;

    #[inline]
    async fn create(&self, _: C) -> Result<Self::Service, Self::InitError> {
        Ok(KeepAliveService::new(self.ka, self.f.clone()))
    }
}

pub struct KeepAliveService<R, E, F> {
    f: F,
    dur: Millis,
    sleep: Sleep,
    expire: Cell<time::Instant>,
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

impl<R, E, F> fmt::Debug for KeepAliveService<R, E, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeepAliveService")
            .field("dur", &self.dur)
            .field("expire", &self.expire)
            .field("f", &std::any::type_name::<F>())
            .finish()
    }
}

impl<R, E, F> Service<R> for KeepAliveService<R, E, F>
where
    F: Fn() -> E,
{
    type Response = R;
    type Error = E;

    async fn ready(&self, _: ServiceCtx<'_, Self>) -> Result<(), Self::Error> {
        let expire = self.expire.get() + time::Duration::from(self.dur);
        if expire <= now() {
            Err((self.f)())
        } else {
            Ok(())
        }
    }

    fn poll(&self, cx: &mut Context<'_>) -> Result<(), Self::Error> {
        match self.sleep.poll_elapsed(cx) {
            Poll::Ready(_) => {
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
    async fn call(&self, req: R, _: ServiceCtx<'_, Self>) -> Result<R, E> {
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

    #[ntex_macros::rt_test2]
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
