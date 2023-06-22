use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use super::ServiceFactory;

/// `MapInitErr` service combinator
pub struct MapInitErr<A, R, C, F, E> {
    a: A,
    f: F,
    e: PhantomData<fn(R, C) -> E>,
}

impl<A, R, C, F, E> MapInitErr<A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
    F: Fn(A::InitError) -> E,
{
    /// Create new `MapInitErr` combinator
    pub(crate) fn new(a: A, f: F) -> Self {
        Self {
            a,
            f,
            e: PhantomData,
        }
    }
}

impl<A, R, C, F, E> Clone for MapInitErr<A, R, C, F, E>
where
    A: Clone,
    F: Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            f: self.f.clone(),
            e: PhantomData,
        }
    }
}

impl<A, R, C, F, E> ServiceFactory<R, C> for MapInitErr<A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
    F: Fn(A::InitError) -> E + Clone,
{
    type Response = A::Response;
    type Error = A::Error;

    type Service = A::Service;
    type InitError = E;
    type Future<'f> = MapInitErrFuture<'f, A, R, C, F, E> where Self: 'f, C: 'f;

    #[inline]
    fn create(&self, cfg: C) -> Self::Future<'_> {
        MapInitErrFuture {
            f: self.f.clone(),
            fut: self.a.create(cfg),
        }
    }
}

pin_project_lite::pin_project! {
    #[must_use = "futures do nothing unless polled"]
    pub struct MapInitErrFuture<'f, A, R, C, F, E>
    where
        A: ServiceFactory<R, C>,
        A: 'f,
        F: Fn(A::InitError) -> E,
        C: 'f,
    {
        f: F,
        #[pin]
        fut: A::Future<'f>,
    }
}

impl<'f, A, R, C, F, E> Future for MapInitErrFuture<'f, A, R, C, F, E>
where
    A: ServiceFactory<R, C>,
    F: Fn(A::InitError) -> E,
{
    type Output = Result<A::Service, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_mut().project();
        this.fut.poll(cx).map_err(|e| (self.project().f)(e))
    }
}

#[cfg(test)]
mod tests {
    use crate::{chain_factory, fn_factory_with_config, into_service, ServiceFactory};

    #[ntex::test]
    async fn map_init_err() {
        let factory = chain_factory(fn_factory_with_config(|err: &bool| {
            let err = *err;
            async move {
                if err {
                    Err(())
                } else {
                    Ok(into_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
                }
            }
        }))
        .map_init_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "err"))
        .clone();

        assert!(factory.create(&true).await.is_err());
        assert!(factory.create(&false).await.is_ok());
    }
}
