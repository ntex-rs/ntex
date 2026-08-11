use std::{fmt, marker::PhantomData};

use super::ServiceFactory;

/// `MapInitError` service combinator
pub struct MapInitErr<A, St, Req, F, Err> {
    a: A,
    f: F,
    e: PhantomData<fn(St, Req) -> Err>,
}

impl<A, St, Req, F, Err> MapInitErr<A, St, Req, F, Err>
where
    A: ServiceFactory<St, Req>,
    F: Fn(A::InitError) -> Err,
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

impl<A, St, Req, F, Err> Clone for MapInitErr<A, St, Req, F, Err>
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

impl<A, St, Req, F, Err> fmt::Debug for MapInitErr<A, St, Req, F, Err>
where
    A: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MapInitErr")
            .field("service", &self.a)
            .field("map", &std::any::type_name::<F>())
            .finish()
    }
}

impl<A, St, Req, F, Err> ServiceFactory<St, Req> for MapInitErr<A, St, Req, F, Err>
where
    A: ServiceFactory<St, Req>,
    F: Fn(A::InitError) -> Err + Clone,
{
    type Res = A::Res;
    type Error = A::Error;

    type Service = A::Service;
    type InitCfg = A::InitCfg;
    type InitError = Err;

    #[inline]
    async fn create(&self, cfg: A::InitCfg) -> Result<Self::Service, Self::InitError> {
        self.a.create(cfg).await.map_err(|e| (self.f)(e))
    }
}

#[cfg(test)]
mod tests {
    use crate::{ServiceFactory, chain_factory, fn_factory_with_config, fn_service};

    #[ntex::test]
    async fn map_init_err() {
        let factory = chain_factory(fn_factory_with_config(|err: &bool| {
            let err = *err;
            async move {
                if err {
                    Err(())
                } else {
                    Ok(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
                }
            }
        }))
        .map_init_err(|()| std::io::Error::other("err"))
        .clone();

        assert!(factory.create(&true).await.is_err());
        assert!(factory.create(&false).await.is_ok());
        let _ = format!("{factory:?}");
    }

    #[ntex::test]
    async fn map_init_err2() {
        let factory = fn_factory_with_config(|err: &bool| {
            let err = *err;
            async move {
                if err {
                    Err(())
                } else {
                    Ok(fn_service(|i: usize| async move { Ok::<_, ()>(i * 2) }))
                }
            }
        })
        .map_init_err(|()| std::io::Error::other("err"))
        .clone();

        assert!(factory.create(&true).await.is_err());
        assert!(factory.create(&false).await.is_ok());
        let _ = format!("{factory:?}");
    }
}
