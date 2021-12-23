use std::{future::Future, marker::PhantomData, pin::Pin, task::Context, task::Poll};

use super::ServiceFactory;

/// `MapInitErr` service combinator
pub struct MapInitErr<A, R, F, E> {
    a: A,
    f: F,
    e: PhantomData<fn(R) -> E>,
}

impl<A, R, F, E> MapInitErr<A, R, F, E>
where
    A: ServiceFactory<R>,
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

impl<A, R, F, E> Clone for MapInitErr<A, R, F, E>
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

impl<A, R, F, E> ServiceFactory<R> for MapInitErr<A, R, F, E>
where
    A: ServiceFactory<R>,
    F: Fn(A::InitError) -> E + Clone,
{
    type Response = A::Response;
    type Error = A::Error;

    type Config = A::Config;
    type Service = A::Service;
    type InitError = E;
    type Future = MapInitErrFuture<A, R, F, E>;

    fn new_service(&self, cfg: A::Config) -> Self::Future {
        MapInitErrFuture {
            fut: self.a.new_service(cfg),
            f: self.f.clone(),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct MapInitErrFuture<A, R, F, E>
    where
        A: ServiceFactory<R>,
        F: Fn(A::InitError) -> E,
    {
        f: F,
        #[pin]
        fut: A::Future,
    }
}

impl<A, R, F, E> Future for MapInitErrFuture<A, R, F, E>
where
    A: ServiceFactory<R>,
    F: Fn(A::InitError) -> E,
{
    type Output = Result<A::Service, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        this.fut.poll(cx).map_err(this.f)
    }
}

#[cfg(test)]
mod tests {
    use crate::{fn_factory_with_config, fn_service, pipeline_factory, ServiceFactory};

    #[ntex::test]
    async fn map_init_err() {
        let factory = pipeline_factory(fn_factory_with_config(|err: bool| async move {
            if err {
                Err(())
            } else {
                Ok(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
            }
        }))
        .map_init_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "err"))
        .clone();

        assert!(factory.new_service(true).await.is_err());
        assert!(factory.new_service(false).await.is_ok());
    }
}
